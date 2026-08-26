// SPDX-License-Identifier: MPL-2.0

use crate::app::AppModel;
use crate::fl;
use crate::message::Message;
use crate::dinit::{ServiceScope, DinitManager};
use crate::types::Page;
use cosmic::prelude::*;

impl AppModel {
    pub fn update_title(&mut self) -> Task<cosmic::Action<Message>> {
        let mut window_title = fl!("app-title");

        if let Some(page) = self.nav.text(self.nav.active()) {
            window_title.push_str(" — ");
            window_title.push_str(page);
        }

        if let Some(id) = self.core.main_window_id() {
            self.set_window_title(window_title, id)
        } else {
            Task::none()
        }
    }
}

impl AppModel {
    /// Handles messages emitted by the application and its widgets.
    pub fn update_message(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::LoadServices(scope) => {
                let mut scope = scope;

                if scope.is_none() {
                    scope = Some(self.current_scope);
                }

                let scope = scope.unwrap();

                // Check if services are already loaded for this scope
                let already_loaded = match scope {
                    ServiceScope::System => !self.system_services.is_empty(),
                    ServiceScope::User => !self.user_services.is_empty(),
                };

                // Only show loader if services aren't already loaded
                if !already_loaded {
                    self.is_loading = true;
                }

                self.current_scope = scope;

                // CACHE HIT: if services for this scope are already loaded,
                // do NOT re-run the (possibly privileged) list command.
                // This is what makes switching System -> User -> System
                // silent: the data is already in memory, no pkexec needed.
                if already_loaded {
                    return Task::none();
                }

                return Task::perform(
                    async move {
                        match DinitManager::new(scope).await {
                            Ok(manager) => match manager.list_services().await {
                                Ok(services) => Some((scope, Ok(services))),
                                Err(e) => Some((scope, Err(e.to_string()))),
                            },
                            Err(e) => Some((scope, Err(e.to_string()))),
                        }
                    },
                    |result| {
                        if let Some((scope, services)) = result {
                            cosmic::Action::from(Message::ServicesLoaded(scope, services))
                        } else {
                            cosmic::Action::from(Message::ServicesLoaded(ServiceScope::System, Ok(Vec::new())))
                        }
                    },
                );
            }

            Message::ServicesLoaded(scope, services) => {
                self.is_loading = false;

                let selected_service_name = self
                    .selected_service
                    .as_ref()
                    .map(|s| s.name.clone());

                let services = match services {
                    Ok(services) => { self.load_error = None; services }
                    Err(e) => { self.load_error = Some(e); Vec::new() }
                };

                match scope {
                    ServiceScope::System => {
                        // If services were already loaded, update only changed items
                        if !self.system_services.is_empty() {
                            for new_service in &services {
                                if let Some(index) = self.system_services.iter().position(|s| s.name == new_service.name) {
                                    // Only update if the service data has changed
                                    let existing_service = &self.system_services[index];
                                    if existing_service.active_state != new_service.active_state
                                        || existing_service.sub_state != new_service.sub_state
                                        || existing_service.load_state != new_service.load_state
                                        || existing_service.unit_file_state != new_service.unit_file_state {
                                        self.system_services[index] = new_service.clone();
                                    }
                                } else {
                                    // New service appeared, add it
                                    self.system_services.push(new_service.clone());
                                }
                            }
                            // Remove services that no longer exist
                            self.system_services.retain(|s| services.iter().any(|new_s| new_s.name == s.name));
                        } else {
                            // First load, replace everything
                            self.system_services = services;
                        }

                        if let Some(name) = selected_service_name {
                            self.selected_service = self.system_services
                                .iter()
                                .find(|s| s.name == name)
                                .cloned();
                        }
                    },
                    ServiceScope::User => {
                        // If services were already loaded, update only changed items
                        if !self.user_services.is_empty() {
                            for new_service in &services {
                                if let Some(index) = self.user_services.iter().position(|s| s.name == new_service.name) {
                                    // Only update if the service data has changed
                                    let existing_service = &self.user_services[index];
                                    if existing_service.active_state != new_service.active_state
                                        || existing_service.sub_state != new_service.sub_state
                                        || existing_service.load_state != new_service.load_state
                                        || existing_service.unit_file_state != new_service.unit_file_state {
                                        self.user_services[index] = new_service.clone();
                                    }
                                } else {
                                    // New service appeared, add it
                                    self.user_services.push(new_service.clone());
                                }
                            }
                            // Remove services that no longer exist
                            self.user_services.retain(|s| services.iter().any(|new_s| new_s.name == s.name));
                        } else {
                            // First load, replace everything
                            self.user_services = services;
                        }

                        if let Some(name) = selected_service_name {
                            self.selected_service = self.user_services
                                .iter()
                                .find(|s| s.name == name)
                                .cloned();
                        }
                    },
                }
            }

            Message::SelectService(service) => {
                self.selected_service = Some(service.clone());
                self.current_page = Page::Details;
                let scope = self.current_scope;
                return Task::perform(
                    async move {
                        let manager = DinitManager::new(scope).await.ok()?;
                        let logs = manager.get_service_logs_unprivileged(&service.name, 100).await.unwrap_or_default();
                        Some(logs)
                    },
                    |result| {
                        if let Some(logs) = result {
                            cosmic::Action::from(Message::LogsLoaded(logs))
                        }
                        else {
                            cosmic::Action::from(Message::LogsLoaded("No logs available".to_string()))
                        }
                    },
                );
            }

            Message::LogsLoaded(logs) => {
                self.service_logs = logs;
            }

            Message::BackToList => {
                self.selected_service = None;
                match self.current_scope {
                    ServiceScope::System => self.current_page = Page::SystemServices,
                    ServiceScope::User => self.current_page = Page::UserServices,
                }
            }

            Message::StartService(name) => {
                if self.action_in_progress {
                    eprintln!("Action already in progress, ignoring {}", name);
                    return cosmic::Task::none();
                }
                self.action_in_progress = true;
                let scope = self.current_scope;
                return Task::perform(
                    async move {
                        if let Ok(manager) = DinitManager::new(scope).await {
                            let _ = manager.start_service(&name).await;
                        }
                    },
                    |_| cosmic::Action::from(Message::ServiceActionComplete),
                );
            }

            Message::StopService(name) => {
                if self.action_in_progress {
                    eprintln!("Action already in progress, ignoring {}", name);
                    return cosmic::Task::none();
                }
                self.action_in_progress = true;
                let scope = self.current_scope;
                return Task::perform(
                    async move {
                        if let Ok(manager) = DinitManager::new(scope).await {
                            let _ = manager.stop_service(&name).await;
                        }
                    },
                    |_| cosmic::Action::from(Message::ServiceActionComplete),
                );
            }

            Message::RestartService(name) => {
                if self.action_in_progress {
                    eprintln!("Action already in progress, ignoring {}", name);
                    return cosmic::Task::none();
                }
                self.action_in_progress = true;
                let scope = self.current_scope;
                return Task::perform(
                    async move {
                        if let Ok(manager) = DinitManager::new(scope).await {
                            let _ = manager.restart_service(&name).await;
                        }
                    },
                    |_| cosmic::Action::from(Message::ServiceActionComplete),
                );
            }

            Message::EnableService(name) => {
                if self.action_in_progress {
                    return cosmic::Task::none();
                }
                self.action_in_progress = true;
                let scope = self.current_scope;
                return Task::perform(
                    async move {
                        if let Ok(manager) = DinitManager::new(scope).await {
                            match manager.enable_service(&name).await {
                                Ok(_) => eprintln!("Successfully enabled: {}", name),
                                Err(e) => eprintln!("Failed to enable {}: {:?}", name, e),
                            }
                        } else {
                            eprintln!("Failed to create DinitManager");
                        }
                    },
                    |_| cosmic::Action::from(Message::ServiceActionComplete),
                );
            }

            Message::DisableService(name) => {
                if self.action_in_progress {
                    eprintln!("Action already in progress, ignoring {}", name);
                    return cosmic::Task::none();
                }
                self.action_in_progress = true;
                eprintln!("DisableService called for: {}", name);
                let scope = self.current_scope;
                return Task::perform(
                    async move {
                        eprintln!("Attempting to disable service: {} with scope: {:?}", name, scope);
                        if let Ok(manager) = DinitManager::new(scope).await {
                            match manager.disable_service(&name).await {
                                Ok(_) => eprintln!("Successfully disabled: {}", name),
                                Err(e) => eprintln!("Failed to disable {}: {:?}", name, e),
                            }
                        } else {
                            eprintln!("Failed to create DinitManager");
                        }
                    },
                    |_| cosmic::Action::from(Message::ServiceActionComplete),
                );
            }

            Message::ServiceActionComplete => {
                self.action_in_progress = false;
                let scope = self.current_scope;
                return Task::perform(async {}, move |_| {
                    cosmic::Action::from(Message::LoadServices(Some(scope)))
                });
            }

            Message::Tick => {
                // Automatic refreshes NEVER touch system scope:
                // system reads go through pkexec/polkit, so refreshing
                // on a timer would keep popping authentication dialogs.
                // System state is refreshed explicitly: on scope entry,
                // after an action completes, or when the user asks.
                // User-scope reads are unprivileged, so refresh those
                // on a gentle interval.
                if self.current_scope != ServiceScope::User {
                    return cosmic::Task::none();
                }

                const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
                let now = std::time::Instant::now();
                if now.duration_since(self.last_refresh) < REFRESH_INTERVAL {
                    return cosmic::Task::none();
                }
                self.last_refresh = now;

                if self.selected_service.is_some() {
                    return Task::perform(async {}, |_| {
                        cosmic::Action::from(Message::RefreshCurrentService)
                    });
                }

                return Task::perform(async {}, |_| {
                    cosmic::Action::from(Message::LoadServices(None))
                });
            }

            Message::RefreshCurrentService => {
                if let Some(service) = &self.selected_service {
                    let base = service.clone();
                    let service_name = service.name.clone();
                    let scope = self.current_scope;
                    return Task::perform(
                        async move {
                            let manager = DinitManager::new(scope).await.ok()?;
                            // IMPORTANT: detail reads must NOT escalate. On system
                            // scope, `dinitctl status` would go through pkexec and
                            // pop a polkit prompt per click. Use unprivileged reads:
                            // status may return "permission denied" for system
                            // services (degrade gracefully), and logs come from the
                            // logfile path read directly (no privilege needed).
                            let (unit_path, pid) = manager.service_details_unprivileged(&service_name).await.unwrap_or_default();
                            let logs = manager.get_service_logs_unprivileged(&service_name, 100).await.unwrap_or_default();
                            let mut updated = base.clone();
                            updated.unit_path = unit_path;
                            updated.pid = pid;
                            Some((updated, logs))
                        },
                        |result| {
                            if let Some((service, logs)) = result {
                                cosmic::Action::from(Message::CurrentServiceRefreshed(Some(service), logs))
                            } else {
                                cosmic::Action::from(Message::CurrentServiceRefreshed(None, String::new()))
                            }
                        },
                    );
                }
            }

            Message::CurrentServiceRefreshed(service, logs) => {
                if let Some(updated_service) = service {
                    self.selected_service = Some(updated_service.clone());
                    self.service_logs = logs;

                    match self.current_scope {
                        ServiceScope::System => {
                            if let Some(index) = self.system_services.iter().position(|s| s.name == updated_service.name) {
                                self.system_services[index] = updated_service;
                            }
                        },
                        ServiceScope::User => {
                            if let Some(index) = self.user_services.iter().position(|s| s.name == updated_service.name) {
                                self.user_services[index] = updated_service;
                            }
                        },
                    }
                }
            }

            Message::ToggleContextPage(context_page) => {
                if self.context_page == context_page {
                    self.core.window.show_context = !self.core.window.show_context;
                } else {
                    self.context_page = context_page;
                    self.core.window.show_context = true;
                }
            }

            Message::SearchFilterChanged(filter) => {
                self.search_filter = filter;
            }

            Message::LaunchUrl(url) => match open::that_detached(&url) {
                Ok(()) => {}
                Err(err) => {
                    eprintln!("failed to open {url:?}: {err}");
                }
            },
        }
        Task::none()
    }
}
