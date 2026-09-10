use super::*;

impl RuntimeState {
    pub(super) async fn inspect_module_install(
        &mut self,
        client: &Client,
        message: &Message,
    ) -> Response {
        let Some(installation) = &self.module_installation else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::InstallUnavailable),
            );
        };
        let acquired = match ModuleSourceAcquirer::new(
            client,
            installation.saved_messages_peer,
            AcquisitionLimits::default(),
        )
        .acquire(message)
        .await
        {
            Ok(acquired) => acquired,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::AttachPackage),
                );
            }
        };
        let config = InspectionConfig {
            staging_root: installation.staging_root.clone(),
            limits: InspectionLimits::default(),
        };
        let now = SystemTime::now();
        let pending = match ModuleInspector::new(&config, OsRandom).inspect(
            acquired,
            now,
            now + DEFAULT_APPROVAL_TTL,
        ) {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(
                    event = "external_module_inspection_rejected",
                    error = %error,
                    "External module package failed safe inspection"
                );
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::UnsafePackage),
                );
            }
        };
        let prefix = self.prefix().to_owned();
        let locale = self.locale();
        match self.module_approvals.issue(pending) {
            Ok((id, _)) => match self.module_approvals.get(id) {
                Ok(plan) => Response::plain_with_locale(
                    locale,
                    render_install_plan(locale, plan, id, &prefix),
                ),
                Err(error) => {
                    tracing::warn!(
                        event = "external_module_approval_plan_unavailable",
                        error = %error,
                        "Issued external module approval could not be read back"
                    );
                    Response::plain_with_locale(locale, lm_text(locale, LmText::PlanUnavailable))
                }
            },
            Err(_) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::PlanUnavailable),
            ),
        }
    }

    pub(super) async fn confirm_module_install(
        &mut self,
        supplied: &crate::commands::ApprovalId,
    ) -> Response {
        let Ok(id) = ApprovalId::parse(supplied.as_str()) else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::ApprovalInvalid),
            );
        };
        let Some(installation) = &self.module_installation else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::InstallUnavailable),
            );
        };
        let module_id = match self.module_approvals.get(id) {
            Ok(plan) => plan.module_id.clone(),
            Err(ApprovalError::Unavailable | ApprovalError::InvalidId) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::ApprovalInvalid),
                );
            }
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::PlanUnavailable),
                );
            }
        };
        if let Some(handle) = &self.external_manager {
            let manager = handle.lock().await;
            if manager.descriptor_by_id(&module_id).is_some() {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_format(self.locale(), LmText::AlreadyRegistered, &module_id, ""),
                );
            }
        }
        let pending = match self.module_approvals.redeem(id) {
            Ok(pending) => pending,
            Err(ApprovalError::Unavailable | ApprovalError::InvalidId) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::ApprovalInvalid),
                );
            }
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::PlanUnavailable),
                );
            }
        };
        let Some(wrapper) = pending.stage.take_wrapper() else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::VerifiedPackageUnavailable),
            );
        };
        let installed = match crate::external_modules::installer::install_staged_module(
            &wrapper,
            &installation.root,
            &module_id,
        ) {
            Ok(installed) => installed,
            Err(error) => {
                if let Err(cleanup) =
                    crate::external_modules::installer::cleanup_redeemed_stage(&wrapper)
                {
                    tracing::warn!(
                        event = "external_module_redeemed_stage_cleanup_failed",
                        wrapper = %cleanup.wrapper.display(),
                        ?cleanup.kind,
                        "Could not remove redeemed external module staging"
                    );
                }
                return match error {
                    crate::external_modules::installer::InstallError::TargetCleanup(_) => {
                        Response::plain_with_locale(self.locale(), lm_text(self.locale(), LmText::InstallRollbackFailed))
                    }
                    crate::external_modules::installer::InstallError::PostInstallValidationFailed { .. } => {
                        Response::plain_with_locale(self.locale(), lm_text(self.locale(), LmText::InstallValidationFailed))
                    }
                    _ => Response::plain_with_locale(self.locale(), lm_text(self.locale(), LmText::InstallFailed)),
                };
            }
        };
        if let Some(handle) = &self.external_manager {
            let registered = {
                let mut manager = handle.lock().await;
                manager.register_installed_descriptor(installed.descriptor)
            };
            if !registered {
                tracing::warn!(
                    event = "external_module_descriptor_registration_duplicate",
                    module_id = %module_id,
                    "Installed external module already has a registered descriptor"
                );
                self.refresh_snapshot().await;
                return Response::plain_with_locale(
                    self.locale(),
                    lm_format(self.locale(), LmText::RegistrationConflict, &module_id, ""),
                );
            }
        }
        self.refresh_snapshot().await;
        Response::plain_with_locale(
            self.locale(),
            lm_format(self.locale(), LmText::InstalledDisabled, &module_id, ""),
        )
    }

    pub(super) fn cancel_module_install(
        &mut self,
        supplied: &crate::commands::ApprovalId,
    ) -> Response {
        match ApprovalId::parse(supplied.as_str()) {
            Ok(id) => match self.module_approvals.revoke(id) {
                Ok(true) => Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::Cancelled),
                ),
                Ok(false) => Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::ApprovalInvalid),
                ),
                Err(_) => Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::PlanUnavailable),
                ),
            },
            Err(_) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::ApprovalInvalid),
            ),
        }
    }
}
pub(super) fn render_install_plan(
    locale: Locale,
    plan: &crate::external_modules::source_inspection::ModuleInstallPlan,
    approval_id: ApprovalId,
    prefix: &str,
) -> String {
    let source = match &plan.source_identity {
        crate::external_modules::source_inspection::SourceIdentity::Archive => {
            lm_label(locale, LmLabel::Archive).to_owned()
        }
        crate::external_modules::source_inspection::SourceIdentity::PinnedRepository(
            repository,
        ) => {
            format!(
                "{} {} @ {}",
                lm_label(locale, LmLabel::Repository),
                repository.repository(),
                repository.revision()
            )
        }
    };
    let capabilities = bounded_list(locale, &plan.capabilities);
    let subscriptions = bounded_list(locale, &plan.subscriptions);
    let methods = bounded_list(locale, &plan.telegram_methods);
    let actions = bounded_list(locale, &plan.actions);
    let warnings = bounded_list(
        locale,
        &plan
            .warnings
            .iter()
            .map(|warning| inspection_warning_text(locale, warning).to_owned())
            .collect::<Vec<_>>(),
    );
    render_lm_install_plan(
        locale,
        LmInstallPlanText {
            source: &source,
            module: &plan.module_id,
            version: &plan.module_version,
            protocol: plan.protocol_version,
            contract_revision: plan.contract_revision,
            entrypoint: &plan.entrypoint,
            default_command: plan
                .default_command
                .as_deref()
                .unwrap_or(lm_label(locale, LmLabel::None)),
            sha256: &plan.archive_digest.as_hex(),
            fingerprint: &plan.fingerprint,
            archive_bytes: plan.archive.archive_bytes,
            file_count: plan.archive.file_count as usize,
            compressed_bytes: plan.archive.compressed_bytes,
            expanded_bytes: plan.archive.expanded_bytes,
            capabilities: &capabilities,
            subscriptions: &subscriptions,
            methods: &methods,
            actions: &actions,
            warnings: &warnings,
            approval_id: &approval_id.to_string(),
            prefix,
        },
    )
}
