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
                Ok(plan) => {
                    let installed = match &self.external_manager {
                        Some(handle) => {
                            let manager = handle.lock().await;
                            manager
                                .descriptor_by_id(&plan.module_id)
                                .map(|descriptor| descriptor.version.clone())
                        }
                        None => None,
                    };
                    let mut text = render_install_plan(locale, plan, id, &prefix);
                    if let Some(current) = installed {
                        text.push('\n');
                        text.push_str(&lm_format(
                            locale,
                            LmText::UpdatePlanned,
                            &plan.module_id,
                            &current,
                        ));
                    }
                    Response::plain_with_locale(locale, text)
                }
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
        // Clone the install root up front: later steps need &mut self while
        // the module installation still borrows it.
        let Some(install_root) = self
            .module_installation
            .as_ref()
            .map(|installation| installation.root.clone())
        else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::InstallUnavailable),
            );
        };
        // Redeem first so the pending quota and the staged wrapper are released
        // for every outcome below, including duplicate and update failures.
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
        let plan = pending.plan;
        let module_id = plan.module_id.clone();
        let Some(wrapper) = pending.stage.take_wrapper() else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::VerifiedPackageUnavailable),
            );
        };
        let redeem_failed_cleanup = |wrapper: &std::path::Path| {
            if let Err(cleanup) =
                crate::external_modules::installer::cleanup_redeemed_stage(wrapper)
            {
                tracing::warn!(
                    event = "external_module_redeemed_stage_cleanup_failed",
                    wrapper = %cleanup.wrapper.display(),
                    ?cleanup.kind,
                    "Could not remove redeemed external module staging"
                );
            }
        };
        if let Some(config) = &self.module_control {
            match control::is_declaratively_managed(&config.declarative_state_path, &module_id) {
                Ok(true) => {
                    redeem_failed_cleanup(&wrapper);
                    return Response::plain_with_locale(
                        self.locale(),
                        lm_text(self.locale(), LmText::Declarative),
                    );
                }
                Err(_) => {
                    redeem_failed_cleanup(&wrapper);
                    return Response::plain_with_locale(
                        self.locale(),
                        lm_text(self.locale(), LmText::PlanUnavailable),
                    );
                }
                Ok(false) => {}
            }
        }
        let (was_enabled, previous_version) = match (&self.module_control, &self.external_manager) {
            (Some(config), Some(handle)) => {
                let state = match ExternalStateStore::load(config.state_path.clone()).await {
                    Ok(state) => state,
                    Err(_) => {
                        redeem_failed_cleanup(&wrapper);
                        return Response::plain_with_locale(
                            self.locale(),
                            lm_text(self.locale(), LmText::StateUnavailable),
                        );
                    }
                };
                let manager = handle.lock().await;
                let previous = manager
                    .descriptor_by_id(&module_id)
                    .map(|descriptor| descriptor.version.clone());
                (state.is_enabled(&module_id), previous)
            }
            _ => (false, None),
        };
        let Some(previous_version) = previous_version else {
            return self
                .confirm_fresh_install(&install_root, &wrapper, &plan, &module_id)
                .await;
        };
        self.confirm_update(
            &install_root,
            wrapper,
            &plan,
            &module_id,
            previous_version,
            was_enabled,
        )
        .await
    }

    async fn confirm_update(
        &mut self,
        install_root: &std::path::Path,
        wrapper: std::path::PathBuf,
        plan: &crate::external_modules::source_inspection::ModuleInstallPlan,
        module_id: &str,
        previous_version: String,
        was_enabled: bool,
    ) -> Response {
        let backups_root = install_root
            .parent()
            .unwrap_or(install_root)
            .join("module-backups");
        if let Some(handle) = &self.external_manager {
            handle.stop_module(module_id).await;
        }
        match crate::external_modules::installer::update_staged_module(
            &wrapper,
            install_root,
            &backups_root,
            module_id,
        ) {
            Ok(updated) => {
                crate::external_modules::installer::prune_module_backups(
                    &backups_root,
                    module_id,
                    &updated.backup_path,
                );
                if let Some(handle) = &self.external_manager {
                    let mut manager = handle.lock().await;
                    manager.replace_descriptor(updated.descriptor);
                }
                if was_enabled && let Some(handle) = &self.external_manager {
                    handle
                        .startup_enabled(&std::collections::BTreeSet::from([module_id.to_owned()]))
                        .await;
                }
                let receipt = crate::external_modules::receipts::receipt_from_plan(
                    plan,
                    Some(previous_version.clone()),
                    SystemTime::now(),
                );
                if let Err(error) = receipt {
                    tracing::warn!(
                        event = "external_module_receipt_build_failed",
                        error = %error,
                        "Updated external module receipt could not be built"
                    );
                } else if let Err(error) = crate::external_modules::receipts::write_receipt(
                    &crate::external_modules::receipts::receipts_root(install_root),
                    &receipt.unwrap(),
                ) {
                    tracing::warn!(
                        event = "external_module_receipt_write_failed",
                        error = %error,
                        "Updated external module receipt could not be persisted"
                    );
                }
                self.refresh_snapshot().await;
                Response::plain_with_locale(
                    self.locale(),
                    lm_format(
                        self.locale(),
                        LmText::Updated,
                        module_id,
                        &format!("v{previous_version} → v{}", plan.module_version),
                    ),
                )
            }
            Err(error) => {
                // The old generation is guaranteed to be back on disk; bring
                // the old process up again under the still-registered
                // descriptor.
                if was_enabled && let Some(handle) = &self.external_manager {
                    handle
                        .startup_enabled(&std::collections::BTreeSet::from([module_id.to_owned()]))
                        .await;
                }
                self.refresh_snapshot().await;
                Response::plain_with_locale(
                    self.locale(),
                    lm_format(
                        self.locale(),
                        LmText::UpdateFailed,
                        module_id,
                        error.reason(),
                    ),
                )
            }
        }
    }

    async fn confirm_fresh_install(
        &mut self,
        install_root: &std::path::Path,
        wrapper: &std::path::Path,
        plan: &crate::external_modules::source_inspection::ModuleInstallPlan,
        module_id: &str,
    ) -> Response {
        let installed = match crate::external_modules::installer::install_staged_module(
            wrapper,
            install_root,
            module_id,
        ) {
            Ok(installed) => installed,
            Err(error) => {
                if let Err(cleanup) =
                    crate::external_modules::installer::cleanup_redeemed_stage(wrapper)
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
        if let Err(error) = crate::external_modules::receipts::write_receipt(
            &crate::external_modules::receipts::receipts_root(install_root),
            &crate::external_modules::receipts::receipt_from_plan(plan, None, SystemTime::now())
                .expect("plan receipt build cannot fail for a validated plan"),
        ) {
            tracing::warn!(
                event = "external_module_receipt_write_failed",
                error = %error,
                "Installed external module receipt could not be persisted"
            );
        }
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
                    lm_format(self.locale(), LmText::RegistrationConflict, module_id, ""),
                );
            }
        }
        self.refresh_snapshot().await;
        Response::plain_with_locale(
            self.locale(),
            lm_format(self.locale(), LmText::InstalledDisabled, module_id, ""),
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
