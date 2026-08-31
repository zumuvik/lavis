use super::*;

impl RuntimeState {
    pub(super) async fn execute_lm(
        &mut self,
        client: &Client,
        message_context: MessageExecutionContext<'_>,
        request: &LmRequest,
    ) -> Response {
        match self.module_approvals.list_pending() {
            Ok(pending) => tracing::debug!(
                event = "external_module_approvals_swept",
                pending = pending.len(),
                "Swept expired external module approvals"
            ),
            Err(error) => tracing::warn!(
                event = "external_module_approvals_sweep_failed",
                error = %error,
                "Could not sweep expired external module approvals"
            ),
        }
        if lm_request_mutates(request)
            && let Err(response) = self.lm_mutation_policy(message_context.clone())
        {
            return response;
        }
        match request {
            LmRequest::Overview | LmRequest::List => self.render_lm_list().await,
            LmRequest::Info { id } => self.lm_info(id).await,
            LmRequest::Logs { id } => self.lm_logs(id).await,
            LmRequest::Doctor { id } => self.lm_doctor(id.as_deref()).await,
            LmRequest::Invalid => self.lm_invalid_usage_response(),
            LmRequest::Install => {
                self.inspect_module_install(client, message_context.message)
                    .await
            }
            LmRequest::Confirm { approval_id } => self.confirm_module_install(approval_id).await,
            LmRequest::Cancel { approval_id } => self.cancel_module_install(approval_id),
            LmRequest::Enable { id } => self.lm_set_enabled(id, true).await,
            LmRequest::Disable { id } => self.lm_set_enabled(id, false).await,
        }
    }

    pub(super) async fn render_lm_list(&self) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::Unavailable),
            );
        };
        let state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::StateUnavailable),
                );
            }
        };
        let list = match control::list_modules(&config.root, &config.declarative_state_path, &state)
        {
            Ok(list) => list,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::ListUnavailable),
                );
            }
        };
        let fresh_snapshot = match &self.external_manager {
            Some(handle) => handle.snapshot().await,
            None => self.external_snapshot.clone(),
        };
        let mut statuses = list
            .modules
            .iter()
            .map(|entry| match &entry.module {
                Some(module) => format!(
                    "• {}\n  {}: {}\n  {}: {}\n  {}: {}\n  {}: {}\n  {}: {}\n  {}: {}\n  {}: {}",
                    module.display_name,
                    lm_label(self.locale(), LmLabel::Id),
                    module.id,
                    lm_label(self.locale(), LmLabel::Version),
                    module.version,
                    lm_label(self.locale(), LmLabel::Status),
                    enabled_label(self.locale(), module.enabled),
                    lm_label(self.locale(), LmLabel::Source),
                    management_label(self.locale(), module.management),
                    lm_label(self.locale(), LmLabel::Runtime),
                    runtime_status_from_snapshot(self.locale(), &fresh_snapshot, &module.id),
                    lm_label(self.locale(), LmLabel::Author),
                    module.author,
                    lm_label(self.locale(), LmLabel::Commands),
                    module.commands.len()
                ),
                None => format!(
                    "• {}\n  {}: {}\n  {}: {}\n  {}: {}",
                    entry
                        .id
                        .as_deref()
                        .unwrap_or(lm_label(self.locale(), LmLabel::InvalidId)),
                    lm_label(self.locale(), LmLabel::Diagnostic),
                    diagnostic_label(self.locale(), entry.diagnostic.as_ref()),
                    lm_label(self.locale(), LmLabel::Status),
                    enabled_label(self.locale(), entry.enabled),
                    lm_label(self.locale(), LmLabel::Source),
                    management_label(self.locale(), entry.management)
                ),
            })
            .collect::<Vec<_>>();
        for id in state.enabled_ids() {
            if !list
                .modules
                .iter()
                .any(|entry| entry.id.as_deref() == Some(id))
            {
                statuses.push(format!(
                    "• {id}\n  {}: {}",
                    lm_label(self.locale(), LmLabel::Status),
                    lm_label(self.locale(), LmLabel::MissingCatalog)
                ));
            }
        }
        if statuses.is_empty() {
            Response::plain_with_locale(
                self.locale(),
                lm_format(self.locale(), LmText::Empty, "", self.prefix()),
            )
        } else {
            Response::plain_with_locale(
                self.locale(),
                format!(
                    "{}\n\n{}",
                    lm_text(self.locale(), LmText::ListHeading),
                    statuses.join("\n\n")
                ),
            )
        }
    }

    pub(super) fn lm_invalid_usage_response(&self) -> Response {
        Response::plain_with_locale(self.locale(), lm_usage(self.locale(), self.prefix()))
    }

    fn lm_mutation_policy(&self, context: MessageExecutionContext<'_>) -> Result<(), Response> {
        self.authorize_sensitive_command(
            SensitiveCommandPolicy::ModuleMutation,
            context,
            self.module_control
                .as_ref()
                .map(|control| control.saved_messages_peer),
        )
    }

    pub(super) async fn lm_logs(&self, id: &str) -> Response {
        let Some(handle) = &self.external_manager else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::RuntimeUnavailable),
            );
        };
        match handle.diagnostic_text(id).await {
            Some(diagnostic) => Response::plain_with_locale(
                self.locale(),
                lm_format(self.locale(), LmText::LastModuleError, id, &diagnostic),
            ),
            None => Response::plain_with_locale(
                self.locale(),
                lm_format(self.locale(), LmText::NoRuntimeError, id, ""),
            ),
        }
    }

    pub(super) async fn lm_doctor(&self, id: Option<&str>) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::Unavailable),
            );
        };
        let state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::StateUnavailable),
                );
            }
        };
        let list = match control::list_modules(&config.root, &config.declarative_state_path, &state)
        {
            Ok(list) => list,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::ListUnavailable),
                );
            }
        };
        let mut lines = Vec::new();
        for entry in &list.modules {
            if let Some(module) = &entry.module {
                if let Some(target) = id
                    && module.id != target
                {
                    continue;
                }
                let runtime = fresh_runtime_status(
                    self.locale(),
                    self.external_manager.as_ref(),
                    &self.external_snapshot,
                    &module.id,
                )
                .await;
                let diagnostic = if let Some(handle) = &self.external_manager {
                    handle.diagnostic_summary(&module.id).await
                } else {
                    None
                };
                lines.push(render_lm_doctor_module(
                    self.locale(),
                    &module.display_name,
                    &module.id,
                    enabled_label(self.locale(), module.enabled),
                    &runtime,
                    management_label(self.locale(), module.management),
                    diagnostic.as_deref(),
                ));
            }
        }
        for enabled in state.enabled_ids() {
            let listed = list
                .modules
                .iter()
                .any(|entry| entry.id.as_deref() == Some(enabled));
            if listed {
                continue;
            }
            if let Some(target) = id
                && enabled != target
            {
                continue;
            }
            lines.push(render_lm_doctor_missing_catalog(self.locale(), enabled));
        }
        let Some(target) = id else {
            return Response::plain_with_locale(
                self.locale(),
                render_lm_doctor_report(self.locale(), None, &lines),
            );
        };
        if lines.is_empty() {
            return Response::plain_with_locale(
                self.locale(),
                lm_format(self.locale(), LmText::NotFound, target, ""),
            );
        }
        Response::plain_with_locale(
            self.locale(),
            render_lm_doctor_report(self.locale(), Some(target), &lines),
        )
    }

    pub(super) async fn lm_info(&self, id: &str) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::Unavailable),
            );
        };
        let state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::StateUnavailable),
                );
            }
        };
        let diagnostic = if let Some(handle) = &self.external_manager {
            handle.diagnostic_text(id).await
        } else {
            None
        };
        match control::module_info(&config.root, &config.declarative_state_path, &state, id) {
            Ok(module) => {
                let locale = self.locale();
                let capabilities = capabilities_label(locale, &module.capabilities);
                let commands = commands_label(locale, &module.commands);
                let runtime = fresh_runtime_status(
                    locale,
                    self.external_manager.as_ref(),
                    &self.external_snapshot,
                    &module.id,
                )
                .await;
                Response::plain_with_locale(
                    self.locale(),
                    render_lm_info(
                        locale,
                        LmInfoResponse {
                            display_name: &module.display_name,
                            id: &module.id,
                            version: &module.version,
                            author: &module.author,
                            enabled: enabled_label(locale, module.enabled),
                            management: management_label(locale, module.management),
                            entrypoint: &module.entrypoint,
                            protocol_version: module.protocol_version,
                            capabilities: &capabilities,
                            commands: &commands,
                            runtime: &runtime,
                            diagnostic: diagnostic.as_deref(),
                        },
                    ),
                )
            }
            Err(control::ModuleControlError::InvalidInstalledModule) => {
                Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::InvalidManifest),
                )
            }
            Err(control::ModuleControlError::ModuleNotInstalled) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::NotInstalled),
            ),
            Err(_) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::NotInstalledOrUnavailable),
            ),
        }
    }

    async fn lm_set_enabled(&self, id: &str, enabled: bool) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::Unavailable),
            );
        };
        let mut state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::StateUnavailable),
                );
            }
        };
        let result = if enabled {
            control::enable_module(&config.root, &config.declarative_state_path, &mut state, id)
                .await
        } else {
            control::disable_module(&config.root, &config.declarative_state_path, &mut state, id)
                .await
        };
        match result {
            Ok(operation) if operation.changed => Response::plain_with_locale(
                self.locale(),
                lm_state_text(
                    self.locale(),
                    LmText::StateChanged,
                    &operation.module.display_name,
                    enabled_label(self.locale(), enabled),
                    self.prefix(),
                ),
            ),
            Ok(operation) => Response::plain_with_locale(
                self.locale(),
                lm_state_text(
                    self.locale(),
                    LmText::StateUnchanged,
                    &operation.module.display_name,
                    enabled_label(self.locale(), enabled),
                    self.prefix(),
                ),
            ),
            Err(control::ModuleControlError::DeclarativelyManaged) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::Declarative),
            ),
            Err(_) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::StateChangeFailed),
            ),
        }
    }
}
pub(super) fn lm_request_mutates(request: &LmRequest) -> bool {
    matches!(
        request,
        LmRequest::Install
            | LmRequest::Confirm { .. }
            | LmRequest::Cancel { .. }
            | LmRequest::Enable { .. }
            | LmRequest::Disable { .. }
    )
}

pub(super) fn lm_usage(locale: Locale, prefix: &str) -> String {
    lm_text(locale, LmText::Usage).replace("{prefix}", prefix)
}
