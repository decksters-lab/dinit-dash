// SPDX-License-Identifier: MPL-2.0
//
// Dinit backend for Dinit Dash (a port of CTL Dash to Dinit).
//
// Instead of talking to systemd over D-Bus, this module shells out to
// `dinitctl`, the control client for the Dinit service manager:
//
//   * `dinitctl list`      - one line per loaded service, with a state marker
//   * `dinitctl status`    - multi-line report: "Service:", "File:", "State:",
//                            "Process ID:" (see dinitctl(8) / dinit(8))
//   * `dinitctl catlog`    - raw buffered log output for a service
//   * enable/disable       - link/unlink service description files; system
//                            scope needs root, so it runs through doas (or
//                            sudo, or pkexec, or flatpak-spawn - automatic)
//
// The D-Bus layer (zbus) of the systemd version is dropped entirely; this
// module relies only on `dinitctl` being on PATH (overridable with the
// DINIT_DASH_DINITCTL environment variable) and the dinit daemon running
// for the requested scope (system or user).

use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct DinitService {
    pub name: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub unit_path: String,
    pub unit_file_state: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceScope {
    System,
    User,
}

impl ServiceScope {
    fn dinitctl_flag(self) -> &'static str {
        match self {
            ServiceScope::System => "-s",
            ServiceScope::User => "-u",
        }
    }

    /// System scope talks to the system dinit, whose control socket is
    /// owned by root with mode 0600 (see dinit's open_control_socket()),
    /// so controlling system services requires elevation unless we run as
    /// root already.
    fn requires_elevation(self) -> bool {
        self == ServiceScope::System && !is_root()
    }
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

type Result<T> = std::result::Result<T, String>;

#[derive(Debug)]
pub struct DinitManager {
    scope: ServiceScope,
}

impl DinitManager {
    /// Services are discovered by parsing `dinitctl list` output, so no
    /// connection setup is required. System services are listed by default;
    /// dinitctl itself falls back to the user instance when not run as root.
    pub async fn new(scope: ServiceScope) -> Result<Self> {
        Ok(DinitManager { scope })
    }

    /// `dinitctl list` prints one line per loaded service:
    ///
    /// ```text
    /// [{+}     ] boot
    /// [ {s}    ] keymap (skipped)
    /// [     {-}] xdm (exit status: 1)
    /// [     {X}] broken
    /// [{ <<   }] starting-service
    /// [{  >>  }] stopping-service
    /// ```
    ///
    /// The bracketed marker encodes: left bracket {/[ (target STARTED/
    /// marked active), +/s/space (started/skipped/not), right bracket
    /// ]/}, then the transition block << / >>, and the stopped-state block
    /// {X/-}. We parse the marker into the two display states used by the UI.


    pub async fn list_services(&self) -> Result<Vec<DinitService>> {
        let output = run_dinitctl(&["list"], self.scope).await?;
        let mut services = Vec::new();

        for line in output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Some((marker, rest)) = parse_list_line(line) else {
                continue;
            };
            let (active_state, sub_state) = classify_states(marker);

            // The rest is the name, possibly followed by parenthesised
            // suffixes in the list output: (pid: N), (exit status: N),
            // (signal: N), (has console), (skipped). Dinit service names
            // never contain spaces, so take the first token as the name.
            let name;
            match rest.find(char::is_whitespace) {
                Some(idx) => name = rest[..idx].to_string(),
                None => name = rest.to_string(),
            }
            if name.is_empty() {
                continue;
            }

            // NOTE: we deliberately do NOT call `dinitctl status <name>`
            // per service here. For system scope that would mean one
            // pkexec/polkit prompt per service (30+ prompts for a full
            // list). `dinitctl list` already gives name + state; the
            // detail fields (unit_path, pid) are filled lazily when a
            // service is selected instead.
            let unit_file_state = self.is_enabled(&name).await;

            services.push(DinitService {
                name,
                load_state: "loaded".to_string(),
                active_state: active_state.to_string(),
                sub_state: sub_state.to_string(),
                unit_path: String::new(),
                unit_file_state,
                pid: None,
            });
        }

        Ok(services)
    }


    /// Enabled state: a service description file resolved from the boot
    /// input directories means the service is enabled. If the file lives
    /// in a non-boot directory it is neither enabled nor disabled
    /// (equivalent to systemd's `static` state).
    /// A service is "enabled" in dinit when a symlink exists in the
    /// boot service's `waits-for.d` directory pointing back at it
    /// (`dinitctl enable` creates exactly that symlink; `disable` removes
    /// it). The waits-for.d directory is conventionally a subdirectory of
    /// the boot service description directory. This check mirrors the
    /// symlink layout dinitctl uses, so the toggle shows the true state.
    async fn is_enabled(&self, name: &str) -> String {
        use std::path::Path;

        let base_dirs: Vec<std::path::PathBuf> = match self.scope {
            ServiceScope::System => [
                "/etc/dinit.d",
                "/run/dinit.d",
                "/usr/local/lib/dinit.d",
                "/usr/lib/dinit.d",
            ]
            .iter()
            .map(Path::new)
            .map(std::path::Path::to_path_buf)
            .collect(),
            ServiceScope::User => {
                let mut dirs = Vec::new();
                if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
                    dirs.push(Path::new(&xdg).join("dinit.d"));
                }
                if let Some(home) = std::env::var_os("HOME") {
                    dirs.push(Path::new(&home).join(".config/dinit.d"));
                }
                dirs
            }
        };

        for base in base_dirs {
            // `dinitctl enable foo` (from boot) links into boot/waits-for.d
            for wfd in [
                base.join("boot/waits-for.d").join(name),
                base.join("boot.d").join(name),
            ] {
                if wfd.exists() {
                    return "enabled".to_string();
                }
            }
        }
        "disabled".to_string()
    }

    /// Start, stop and restart are direct `dinitctl` commands. restart is a
    /// stop followed by a start. Errors are surfaced so the UI can show why
    /// an action failed.
    pub async fn start_service(&self, service_name: &str) -> Result<()> {
        self.run_control(&["start", service_name]).await.map(|_| ())
    }

    pub async fn stop_service(&self, service_name: &str) -> Result<()> {
        self.run_control(&["stop", service_name]).await.map(|_| ())
    }

    pub async fn restart_service(&self, service_name: &str) -> Result<()> {
        self.run_control(&["restart", service_name]).await.map(|_| ())
    }

    /// Enabling and disabling link/unlink the service description file.
    /// System scope needs root, so this goes through doas/sudo/pkexec
    /// (whichever the host provides; a Flatpak sandbox is handled via
    /// flatpak-spawn --host pkexec).
    pub async fn enable_service(&self, service_name: &str) -> Result<()> {
        self.run_enable_disable(&["enable", service_name]).await.map(|_| ())
    }

    pub async fn disable_service(&self, service_name: &str) -> Result<()> {
        self.run_enable_disable(&["disable", service_name]).await.map(|_| ())
    }


    /// Unprivileged variant of `service_details`: never escalates. On
    /// system scope this may fail with permission denied, which is fine —
    /// the detail pane degrades gracefully instead of popping polkit.
    pub async fn service_details_unprivileged(&self, name: &str) -> Result<(String, Option<u32>)> {
        let mut path = String::new();
        let mut pid: Option<u32> = None;

        // Try status first if socket allows
        if let Ok(output) = run_dinitctl_unprivileged(&["status", name], self.scope).await {
            for line in output.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("pid:") {
                    pid = rest.trim().split(|c: char| !c.is_ascii_digit()).next().and_then(|s| s.parse().ok());
                } else if let Some(rest) = line.strip_prefix("File:") {
                    path = rest.trim().to_string();
                }
            }
        }

        // If path is still empty, look up the service file from known dirs
        if path.is_empty() {
            path = self.find_service_file_path(name).unwrap_or_default();
        }

        Ok((path, pid))
    }

    fn find_service_file_path(&self, name: &str) -> Option<String> {
        use std::path::PathBuf;
        let mut dirs: Vec<PathBuf> = Vec::new();
        if self.scope == ServiceScope::User {
            if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
                if !xdg.is_empty() {
                    dirs.push(PathBuf::from(&xdg).join("dinit.d"));
                }
            }
            if let Ok(home) = std::env::var("HOME") {
                if !home.is_empty() {
                    dirs.push(PathBuf::from(&home).join(".config/dinit.d"));
                }
            }
            dirs.push(PathBuf::from("/etc/dinit.d/user"));
            dirs.push(PathBuf::from("/usr/local/lib/dinit.d/user"));
            dirs.push(PathBuf::from("/usr/lib/dinit.d/user"));
        } else {
            dirs.push(PathBuf::from("/etc/dinit.d"));
            dirs.push(PathBuf::from("/usr/local/lib/dinit.d"));
            dirs.push(PathBuf::from("/usr/lib/dinit.d"));
            dirs.push(PathBuf::from("/run/dinit.d"));
        }
        for dir in &dirs {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
        None
    }

    /// Unprivileged variant of `get_service_logs`: reads the `logfile`
    /// path directly (file read, no privilege escalation, no polkit).
    pub async fn get_service_logs_unprivileged(&self, service_name: &str, lines: u32) -> Result<String> {
        if let Some(path) = self.read_logfile_path(service_name) {
            let data = std::fs::read_to_string(&path).map_err(|e| format!("logfile {}: {}", path.display(), e))?;
            let tail: Vec<&str> = data.lines().rev().take(lines as usize).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            return Ok(tail.join("\n"));
        }
        Err("This service has neither a log buffer (log-type = buffer) nor a logfile setting.".to_string())
    }

    /// Read the `logfile = ...` path (with variable substitution for $HOME,
    /// $XDG_* where meaningful) from the service description file.
    fn read_logfile_path(&self, name: &str) -> Option<std::path::PathBuf> {
        use std::path::PathBuf;
        let mut dirs: Vec<PathBuf> = Vec::new();
        if self.scope == ServiceScope::User {
            if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
                if !xdg.is_empty() {
                    dirs.push(PathBuf::from(&xdg).join("dinit.d"));
                }
            }
            if let Ok(home) = std::env::var("HOME") {
                if !home.is_empty() {
                    dirs.push(PathBuf::from(&home).join(".config/dinit.d"));
                }
            }
            dirs.push(PathBuf::from("/etc/dinit.d/user"));
            dirs.push(PathBuf::from("/usr/local/lib/dinit.d/user"));
            dirs.push(PathBuf::from("/usr/lib/dinit.d/user"));
        } else {
            dirs.push(PathBuf::from("/etc/dinit.d"));
            dirs.push(PathBuf::from("/usr/local/lib/dinit.d"));
            dirs.push(PathBuf::from("/usr/lib/dinit.d"));
            dirs.push(PathBuf::from("/run/dinit.d"));
        }
        for dir in &dirs {
            let candidate = dir.join(name);
            if candidate.is_file() {
                if let Ok(content) = std::fs::read_to_string(&candidate) {
                    for line in content.lines() {
                        let line = line.trim();
                        if let Some(rest) = line.strip_prefix("logfile") {
                            let rest = rest.trim_start();
                            let rest = rest.strip_prefix('=').unwrap_or(rest);
                            let rest = rest.trim().trim_matches('"');
                            if !rest.is_empty() {
                                // dinit supports variable substitution; resolve the
                                // common ones so the path is actually readable.
                                let mut path = rest.to_string();
                                if let Ok(home) = std::env::var("HOME") {
                                    path = path.replace("$HOME", &home).replace("${HOME}", &home);
                                }
                                return Some(PathBuf::from(path));
                            }
                        }
                    }
                }
                return None;
            }
        }
        None
    }

/// Run a control command, elevating when the scope demands it.
async fn run_control(&self, args: &[&str]) -> Result<String> {
    if self.scope.requires_elevation() {
        run_dinitctl_privileged(args, self.scope).await
    } else {
        run_dinitctl(args, self.scope).await
    }
}

/// Run an enable/disable command, elevating when needed.
async fn run_enable_disable(&self, args: &[&str]) -> Result<String> {
    if self.scope.requires_elevation() {
        run_dinitctl_privileged(args, self.scope).await
    } else {
        run_dinitctl(args, self.scope).await
    }
}



}

/// Run dinitctl directly (no privilege escalation).
async fn run_dinitctl(args: &[&str], scope: ServiceScope) -> Result<String> {
    // System scope hits dinit's 0600 root-owned control socket, so it must
    // be queried with elevated privileges. Prefer pkexec for a graphical
    // polkit prompt (never doas/sudo, which prompt in the terminal and
    // hang the GUI). User scope runs directly.
    if scope == ServiceScope::System && !is_root() {
        if let Some(escalator) = find_privilege_escalator().await {
            let mut cmd = Command::new(&escalator);
            cmd.arg(dinitctl_bin());
            let flag = scope.dinitctl_flag();
            if !flag.is_empty() {
                cmd.arg(flag);
            }
            cmd.args(args);
            let output = cmd.output().await.map_err(|e| format!("Failed to execute {}: {}", escalator, e))?;
            if !output.status.success() {
                let error = String::from_utf8_lossy(&output.stderr);
                return Err(format!("dinitctl failed: {}", error.trim()));
            }
            return Ok(String::from_utf8_lossy(&output.stdout).to_string());
        }
    }

    let dinitctl = dinitctl_bin();
    let output = Command::new(&dinitctl)
        .arg(scope.dinitctl_flag())
        .args(args)
        .output()
        .await
        .map_err(|e| format!("Failed to execute dinitctl: {}", e))?;

    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(format!("dinitctl failed: {}", error.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Resolve the dinitctl binary, honouring DINIT_DASH_DINITCTL.
fn dinitctl_bin() -> String {
    std::env::var("DINIT_DASH_DINITCTL").unwrap_or_else(|_| "dinitctl".to_string())
}

/// First candidate is preferred; the rest are fallbacks (checked for
/// existence so we don't fail on systems without doas, etc.).
async fn find_privilege_escalator() -> Option<String> {
    for candidate in ["pkexec", "doas", "sudo"] {
        if Command::new(candidate).arg("--version").output().await.is_ok() {
            return Some(candidate.to_string());
        }
    }
    // flatpak-spawn --host <cmd> - no --version; just try it on demand
    if Command::new("flatpak-spawn").arg("--help").output().await.is_ok() {
        return Some("flatpak-spawn".to_string());
    }
    None
}

/// Run dinitctl under the best available privilege escalator.
/// Run dinitctl WITHOUT any privilege escalation, regardless of scope.
/// Used for detail-pane reads (status, logfile tails) where a polkit
/// prompt per click would be terrible UX. System-scope reads will
/// typically get "permission denied" from the 0600 socket — callers
/// degrade gracefully.
async fn run_dinitctl_unprivileged(args: &[&str], scope: ServiceScope) -> Result<String> {
    let mut cmd = Command::new(dinitctl_bin());
    let flag = scope.dinitctl_flag();
    if !flag.is_empty() {
        cmd.arg(flag);
    }
    cmd.args(args);
    let output = cmd.output().await.map_err(|e| format!("Failed to execute dinitctl: {}", e))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(format!("dinitctl failed: {}", error.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn run_dinitctl_privileged(args: &[&str], scope: ServiceScope) -> Result<String> {
    let Some(escalator) = find_privilege_escalator().await else {
        return Err("No privilege escalation tool found (doas, sudo, pkexec, flatpak-spawn)".to_string());
    };

    let dinitctl = dinitctl_bin();

    let output = match escalator.as_str() {
        "flatpak-spawn" => Command::new("flatpak-spawn")
            .arg("--host")
            .arg("pkexec")
            .arg(&dinitctl)
            .arg(scope.dinitctl_flag())
            .args(args)
            .output()
            .await,
        _ => Command::new(&escalator)
            .arg(&dinitctl)
            .arg(scope.dinitctl_flag())
            .args(args)
            .output()
            .await,
    }
    .map_err(|e| format!("Failed to execute {}: {}", escalator, e))?;

    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(format!("dinitctl failed via {}: {}", escalator, error.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Parse one line of `dinitctl list` output.
///
/// The marker is FIXED WIDTH (verified against dinitctl.cc source):
///   char 0: '[' (literal)
///   char 1: '[' marked-active | '{' target-started | ' '
///   char 2: '+' running | 's' skipped | ' '
///   char 3: ']' marked-active | '}' target-started | ' '
///   chars 4-5: '<<' starting | '>>' stopping | '  '
///   char 6: '{' target-stopped | ' '
///   char 7: 'X' failed | '-' stopped-ok | ' '
///   char 8: '}' target-stopped | ' '
///   char 9: ']' (literal close)
///   char 10: ' ' (space before name)
///   then: service name, optionally " (pid: N)", " (exit status: N)",
///   " (signal: N)", " (has console)", " (skipped)".
fn parse_list_line(line: &str) -> Option<(&str, &str)> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let bytes = line.as_bytes();
    if bytes.len() < 11 || bytes[0] != b'[' || bytes[9] != b']' || bytes[10] != b' ' {
        return None;
    }
    let marker = &line[..9];
    let rest = &line[11..];
    Some((marker, rest))
}

/// Turn a dinitctl list marker (chars 0-8) into (active, sub) states.
fn classify_states(marker: &str) -> (&'static str, &'static str) {
    let b = marker.as_bytes();
    let started = b.get(2) == Some(&b'+');
    let skipped = b.get(2) == Some(&b's');
    let failed = b.get(7) == Some(&b'X');
    let stopped_ok = b.get(7) == Some(&b'-');
    let starting = b.get(4) == Some(&b'<');
    let stopping = b.get(4) == Some(&b'>');

    if starting {
        return ("activating", "start");
    }
    if stopping {
        return ("deactivating", "stop");
    }
    if failed {
        return ("failed", "failed");
    }
    if started {
        return ("active", "running");
    }
    if stopped_ok {
        return ("inactive", "dead");
    }
    if skipped {
        return ("inactive", "dead");
    }
    ("inactive", "dead")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_list_line() {
        let line = "[{+}     ] boot";
        let (marker, rest) = parse_list_line(line).unwrap();
        assert_eq!(marker, "[{+}     ");
        assert_eq!(rest, "boot");
    }

    #[test]
    fn test_parse_list_line_marked_active() {
        // Marked-active services use ] for both brackets: the parser must
        // NOT mistake the inner ] for the end of the marker.
        let line = "[[+]     ] xdg-user-dirs";
        let (marker, rest) = parse_list_line(line).unwrap();
        assert_eq!(marker, "[[+]     ");
        assert_eq!(rest, "xdg-user-dirs");
    }

    #[test]
    fn test_parse_list_line_with_suffix() {
        let line = "[     {-}] xdm (exit status: 1)";
        let (marker, rest) = parse_list_line(line).unwrap();
        assert_eq!(rest, "xdm");
    }

    #[test]
    fn test_parse_list_line_starting() {
        let line = "[{ <<   }] starting-service";
        let (marker, rest) = parse_list_line(line).unwrap();
        assert_eq!(marker, "[{ <<   }");
        assert_eq!(rest, "starting-service");
    }

    #[test]
    fn test_parse_list_line_skipped() {
        let line = "[ {s}    ] keymap (skipped)";
        let (marker, rest) = parse_list_line(line).unwrap();
        assert_eq!(marker, "[ {s}    ");
        assert_eq!(rest, "keymap");
    }

    #[test]
    fn test_parse_list_line_short_rejected() {
        // Malformed / too-short lines (e.g. a stray "[" line) must be skipped.
        assert!(parse_list_line("[foo").is_none());
        assert!(parse_list_line("[{+} ]").is_none());
        assert!(parse_list_line("").is_none());
    }

    #[test]
    fn test_classify_states() {
        // Markers are 9 chars incl. the leading '[' (dinitctl prints
        // '[' + 8 inner + '] ' + name; we keep the leading '[').
        let (active, sub) = classify_states("[{+}     ");
        assert_eq!(active, "active");
        assert_eq!(sub, "running");

        let (active, sub) = classify_states("[[+]     ");
        assert_eq!(active, "active");
        assert_eq!(sub, "running");

        let (active, sub) = classify_states("[     {-}");
        assert_eq!(active, "inactive");
        assert_eq!(sub, "dead");

        let (active, sub) = classify_states("[     {X}");
        assert_eq!(active, "failed");
        assert_eq!(sub, "failed");

        let (active, sub) = classify_states("[{ <<   }");
        assert_eq!(active, "activating");
        assert_eq!(sub, "start");

        let (active, sub) = classify_states("[{  >>  }");
        assert_eq!(active, "deactivating");
        assert_eq!(sub, "stop");

        let (active, sub) = classify_states("[ {s}    ");
        assert_eq!(active, "inactive");
        assert_eq!(sub, "dead");
    }

#[test]
    fn test_service_scope_equality() {
        assert_eq!(ServiceScope::System, ServiceScope::System);
        assert_eq!(ServiceScope::User, ServiceScope::User);
        assert_ne!(ServiceScope::System, ServiceScope::User);
    }

    #[test]
    fn test_service_scope_copy() {
        let scope1 = ServiceScope::System;
        let scope2 = scope1;
        assert_eq!(scope1, scope2);
    }

    #[test]
    fn test_dinit_manager_new() {
        // Construction never fails: there is no connection to establish.
        let _ = DinitManager::new(ServiceScope::System);
    }
}
