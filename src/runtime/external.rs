use super::info::fastfetch_response;
use super::*;

impl RuntimeState {
    pub fn external_command_refs(&self) -> &[crate::external_modules::manager::ExternalCommandRef] {
        &self.external_snapshot.command_refs
    }

    pub fn has_active_external_command(&self, name: &str) -> bool {
        self.external_snapshot.active_commands.contains(name)
    }

    pub fn external_descriptors(
        &self,
    ) -> &[crate::external_modules::manifest::ExternalModuleDescriptor] {
        &self.external_snapshot.descriptors
    }

    pub fn resolve_external(&self, name: &str, args: &str) -> Option<Action> {
        if !self.has_active_external_command(name) {
            return None;
        }
        let dot = name.find('.')?;
        let module_id = name[..dot].to_owned();
        let command_name = name[dot + 1..].to_owned();
        Some(Action::External(ExternalInvocation {
            module_id,
            command_name,
            arguments: if args.is_empty() {
                String::new()
            } else {
                args.to_owned()
            },
            argument_entities: Vec::new(),
        }))
    }

    pub fn resolve_external_default(&self, name: &str, args: &str) -> Option<Action> {
        let command_name = self.external_snapshot.active_defaults.get(name)?.clone();
        Some(Action::External(ExternalInvocation {
            module_id: name.to_owned(),
            command_name,
            arguments: args.to_owned(),
            argument_entities: Vec::new(),
        }))
    }

    pub fn has_external_module(&self, module_id: &str) -> bool {
        self.external_snapshot
            .descriptors
            .iter()
            .any(|descriptor| descriptor.id == module_id)
    }

    pub fn external_has_capability(
        &self,
        module_id: &str,
        capability: crate::external_modules::manifest::ExternalCapability,
    ) -> bool {
        self.external_snapshot
            .descriptors
            .iter()
            .find(|descriptor| descriptor.id == module_id)
            .is_some_and(|descriptor| descriptor.capabilities.contains(&capability))
    }

    async fn execute_external(
        &mut self,
        invocation: &ExternalInvocation,
        message_context: MessageExecutionContext<'_>,
    ) -> Response {
        let locale = self.locale();
        let handle = match self.external_manager.clone() {
            Some(h) => h,
            None => {
                return Response::plain_with_locale(
                    self.locale(),
                    external_command_text(
                        locale,
                        ExternalCommandText::RuntimeUnavailable,
                        "",
                        None,
                    ),
                );
            }
        };
        let result = handle
            .execute_with_message_context(
                &invocation.module_id,
                &invocation.command_name,
                &invocation.arguments,
                &invocation.argument_entities,
                message_context.message.clone(),
                message_context.message.text().to_owned(),
                message_context.replied.clone(),
                message_context.message.peer_id(),
                message_context.authored_by_self,
            )
            .await;
        let response = match &result {
            Ok(text) => {
                let found = self
                    .external_snapshot
                    .descriptors
                    .iter()
                    .find(|d| d.id == invocation.module_id);
                match found {
                    Some(desc) => {
                        let first_notice = self.external_warnings_announced.insert(desc.id.clone());
                        Response::external_result(
                            self.locale(),
                            text,
                            &desc.display_name,
                            &desc.id,
                            &desc.version,
                            first_notice,
                        )
                    }
                    None => missing_descriptor_response(locale, text, &invocation.module_id),
                }
            }
            Err(ExternalError::Unavailable) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::Unavailable,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(ExternalError::ExecutionTimeout) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::Timeout,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(ExternalError::ProtocolDecode) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::ProtocolDecode,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(ExternalError::WrongRequestId) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::WrongRequestId,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(ExternalError::ModuleError) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::ModuleError,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(ExternalError::ResultTooLarge) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::ResultTooLarge,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(error) => {
                tracing::warn!(
                    event = "external_command_error",
                    module_id = %invocation.module_id,
                    command = %invocation.command_name,
                    error = %error,
                    "External command failed"
                );
                Response::plain_with_locale(
                    self.locale(),
                    external_command_text(
                        locale,
                        ExternalCommandText::GenericError,
                        &invocation.module_id,
                        Some(&error.to_string()),
                    ),
                )
            }
        };
        if result.is_err() {
            self.refresh_snapshot().await;
        }
        response
    }

    pub(crate) async fn execute(
        &mut self,
        client: &Client,
        action: &Action,
        message_id: i32,
        peer_id: PeerId,
        message_context: MessageExecutionContext<'_>,
    ) -> RuntimeExecution {
        self.recognized_commands = self.recognized_commands.saturating_add(1);
        let prefix = self.prefix().to_owned();
        if let Action::Start(request) = action {
            return self.execute_start(client, request, peer_id).await;
        }
        if let Action::Setup(request) = action {
            return self.execute_setup(client, request, peer_id).await;
        }
        if let Action::Info = action {
            return self.execute_info();
        }
        match action {
            Action::Language(request) => self.execute_language(request).await,
            Action::Ping => match telegram_ping(client, message_id).await {
                Ok(latency) => Response::plain_with_locale(
                    self.locale(),
                    ping_text(self.locale(), PingText::Success, &format_latency(latency)),
                ),
                Err(error) => {
                    log_ping_failure(action, message_id, &error);
                    Response::plain_with_locale(
                        self.locale(),
                        runtime_text(self.locale(), RuntimeText::PingFailed),
                    )
                }
            },
            Action::Stats => {
                let telegram = match telegram_ping(client, message_id).await {
                    Ok(latency) => format_latency(latency),
                    Err(error) => {
                        log_ping_failure(action, message_id, &error);
                        stats_text(self.locale(), StatsText::Unavailable).to_owned()
                    }
                };
                let proc_stats = read_proc_stats().await;
                log_unavailable_proc_stats(&proc_stats);
                Response::plain_with_locale(
                    self.locale(),
                    format_stats(
                        self.locale(),
                        &telegram,
                        self.started_at.elapsed(),
                        &proc_stats,
                        self.recognized_commands,
                    ),
                )
            }
            Action::Info => unreachable!("info actions return before response dispatch"),
            Action::Help(request) => {
                let rendered = render_with_external_locale(
                    request,
                    &prefix,
                    &self.aliases,
                    self.external_command_refs(),
                    self.external_descriptors(),
                    self.locale(),
                );
                if rendered.entity_fallback {
                    tracing::warn!(
                        event = "help_entity_fallback",
                        "Help formatting was unavailable"
                    );
                }
                rendered.response
            }
            Action::Fastfetch(arguments) => fastfetch_response(
                fastfetch::run(self.locale(), arguments).await,
                self.locale(),
                &prefix,
            ),
            Action::Alias(request) => self.execute_alias(request, &prefix).await,
            Action::Prefix(request) => self.execute_prefix(request).await,
            Action::Modules(request) => self.execute_modules(request, &prefix),
            Action::Lm(request) => self.execute_lm(client, message_context, request).await,
            Action::Reboot => return self.execute_reboot(message_context),
            Action::Setup(_) => unreachable!("setup actions return before response dispatch"),
            Action::Start(_) => unreachable!("start actions return before response dispatch"),
            Action::External(invocation) => {
                self.execute_external(invocation, message_context).await
            }
        }
        .into()
    }
}
