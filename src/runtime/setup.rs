use super::*;

pub(super) struct SetupCoordinator {
    pub(super) state_path: PathBuf,
    pub(super) token_path: PathBuf,
    pub(super) saved_messages_peer: PeerId,
    pub(super) botfather_peer: Option<PeerId>,
    pub(super) phase: SetupPhase,
}

pub(super) enum SetupPhase {
    Idle,
    AwaitingUsername {
        automatic: bool,
        deadline: Instant,
    },
    AwaitingConfirmation {
        username: UsernameCandidate,
        automatic: bool,
        attempts: u8,
        deadline: Instant,
    },
    Running {
        flow: CompanionSetup,
        transport: GrammersTelegramSetup,
        automatic: bool,
        attempts: u8,
        deadline: Instant,
    },
}

pub(super) struct BotFatherOutcome {
    pub(super) response: Option<Response>,
    pub(super) provision: Option<ProvisionRequest>,
}

impl RuntimeState {
    pub(super) async fn execute_setup(
        &mut self,
        client: &Client,
        request: &SetupRequest,
        peer: PeerId,
    ) -> RuntimeExecution {
        let locale = self.locale();
        let Some(setup) = &mut self.setup else {
            return Response::plain_with_locale(
                self.locale(),
                setup_text(locale, SetupText::Unavailable),
            )
            .into();
        };
        if peer != setup.saved_messages_peer {
            return Response::plain_with_locale(
                self.locale(),
                setup_text(locale, SetupText::SavedMessagesOnly),
            )
            .into();
        }
        let execution = setup.handle_command(client, request, locale).await;
        if setup.botfather_peer.is_some() {
            self.external_projection_permitted = true;
        }
        execution
    }
}
impl SetupCoordinator {
    pub(super) fn is_active(&self) -> bool {
        !matches!(self.phase, SetupPhase::Idle)
    }

    pub(super) async fn handle_command(
        &mut self,
        client: &Client,
        request: &SetupRequest,
        locale: Locale,
    ) -> RuntimeExecution {
        if matches!(
            request,
            SetupRequest::Start | SetupRequest::Auto | SetupRequest::Username(_)
        ) {
            if self.is_active() {
                return Response::plain(setup_text(locale, SetupText::AlreadyActive)).into();
            }
            if self.has_created_bot().await {
                return Response::plain(setup_text(locale, SetupText::ExistingBot)).into();
            }
        }
        if matches!(request, SetupRequest::Repair) {
            return self.repair(client, locale).await;
        }
        match request {
            SetupRequest::Status => self.status(locale).await,
            SetupRequest::Cancel => {
                if !self.is_active() {
                    return Response::plain(setup_text(locale, SetupText::NoActiveSetup)).into();
                }
                self.phase = SetupPhase::Idle;
                Response::plain(setup_text(locale, SetupText::Cancelled))
            }
            SetupRequest::Start => {
                self.phase = SetupPhase::AwaitingUsername {
                    automatic: false,
                    deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
                };
                Response::plain(setup_text(locale, SetupText::UsernamePrompt))
            }
            SetupRequest::Auto => match crate::setup::generate_candidate() {
                Ok(username) => self.confirm_or_start(username, true, 1, locale).await,
                Err(_) => Response::plain(setup_text(locale, SetupText::UsernameGenerationFailed)),
            },
            SetupRequest::Username(value) => match crate::setup::validate_username(value) {
                Ok(username) => self.confirm_or_start(username, false, 1, locale).await,
                Err(_) => Response::plain(setup_text(locale, SetupText::UsernameInvalid)),
            },
            SetupRequest::Repair => unreachable!("repair returns a provisioning request"),
            SetupRequest::Invalid => Response::plain(setup_text(locale, SetupText::Usage)),
        }
        .into()
    }

    pub(super) async fn confirm_or_start(
        &mut self,
        username: UsernameCandidate,
        automatic: bool,
        attempts: u8,
        locale: Locale,
    ) -> Response {
        self.phase = SetupPhase::AwaitingConfirmation {
            username: username.clone(),
            automatic,
            attempts,
            deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
        };
        let plan = setup_text(locale, SetupText::Plan)
            .replace("{username}", username.display())
            .replace("{display_name}", crate::setup_telegram::DISPLAY_NAME);
        Response::plain(plan)
    }

    pub(super) async fn handle_input(
        &mut self,
        client: &Client,
        text: &str,
        locale: Locale,
    ) -> Response {
        if matches!(
            crate::setup::parse_confirmation(text),
            Some(crate::setup::Confirmation::Cancelled)
        ) {
            self.phase = SetupPhase::Idle;
            return Response::plain(setup_text(locale, SetupText::Cancelled));
        }
        match &self.phase {
            SetupPhase::AwaitingUsername { .. } => self.handle_username_input(text, locale).await,
            SetupPhase::AwaitingConfirmation {
                username,
                automatic,
                attempts,
                ..
            } => {
                if matches!(
                    crate::setup::parse_confirmation(text),
                    Some(crate::setup::Confirmation::Confirmed)
                ) {
                    self.start_flow(client, username.clone(), *automatic, *attempts, locale)
                        .await
                } else {
                    Response::plain(setup_text(locale, SetupText::ConfirmOrCancel))
                }
            }
            _ => Response::plain(setup_text(locale, SetupText::WaitingBotFather)),
        }
    }

    pub(super) async fn handle_username_input(&mut self, text: &str, locale: Locale) -> Response {
        let automatic = match &self.phase {
            SetupPhase::AwaitingUsername { automatic, .. } => *automatic,
            _ => return Response::plain(setup_text(locale, SetupText::WaitingBotFather)),
        };
        let generated = matches!(text.trim().to_ascii_lowercase().as_str(), "-" | "auto");
        let username = match generated {
            true => crate::setup::generate_candidate()
                .map_err(|_| crate::setup::UsernameError::InvalidCharactersOrLength),
            false => crate::setup::validate_username(text.trim()),
        };
        match username {
            Ok(username) => {
                self.confirm_or_start(username, automatic || generated, 1, locale)
                    .await
            }
            Err(_) => Response::plain(setup_text(locale, SetupText::UsernameInvalid)),
        }
    }

    pub(super) async fn start_flow(
        &mut self,
        client: &Client,
        username: UsernameCandidate,
        automatic: bool,
        attempts: u8,
        locale: Locale,
    ) -> Response {
        let Ok((transport, peer)) = GrammersTelegramSetup::resolve(client).await else {
            return Response::plain(setup_text(locale, SetupText::BotFatherUnavailable));
        };
        let mut flow =
            CompanionSetup::new(username, self.state_path.clone(), self.token_path.clone());
        if flow.start(&transport).await.is_err() {
            return Response::plain(setup_text(locale, SetupText::BotFatherStartFailed));
        }
        self.botfather_peer = Some(peer);
        self.phase = SetupPhase::Running {
            flow,
            transport,
            automatic,
            attempts,
            deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
        };
        Response::plain(setup_text(locale, SetupText::Started))
    }

    pub(super) async fn handle_botfather_reply(
        &mut self,
        client: &Client,
        text: &str,
        locale: Locale,
    ) -> BotFatherOutcome {
        let SetupPhase::Running {
            flow,
            transport,
            automatic,
            attempts,
            deadline,
        } = &mut self.phase
        else {
            return BotFatherOutcome {
                response: None,
                provision: None,
            };
        };
        let api = match HttpBotApi::new() {
            Ok(api) => api,
            Err(_) => {
                self.phase = SetupPhase::Idle;
                return BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotCheckUnavailable,
                    ))),
                    provision: None,
                };
            }
        };
        match flow.on_botfather_reply(text, transport, &api).await {
            Ok(BotFatherProgress::Pending) => {
                *deadline = Instant::now() + SETUP_STAGE_TIMEOUT;
                BotFatherOutcome {
                    response: None,
                    provision: None,
                }
            }
            Ok(BotFatherProgress::ProvisionReady) => {
                let request = flow.provision_request(client.clone());
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: None,
                    provision: Some(request),
                }
            }
            Ok(BotFatherProgress::UsernameOccupied) if *automatic && *attempts < 10 => {
                let next_attempt = *attempts + 1;
                self.phase = SetupPhase::Idle;
                match crate::setup::generate_candidate() {
                    Ok(username) => {
                        let response = self
                            .start_flow(client, username, true, next_attempt, locale)
                            .await;
                        BotFatherOutcome {
                            response: Some(response),
                            provision: None,
                        }
                    }
                    Err(_) => BotFatherOutcome {
                        response: Some(Response::plain(setup_text(
                            locale,
                            SetupText::UsernameGenerationFailed,
                        ))),
                        provision: None,
                    },
                }
            }
            Ok(BotFatherProgress::UsernameOccupied | BotFatherProgress::UsernameInvalid) => {
                self.phase = SetupPhase::AwaitingUsername {
                    automatic: false,
                    deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
                };
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotUsernameRejected,
                    ))),
                    provision: None,
                }
            }
            Ok(BotFatherProgress::LimitReached) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotLimitReached,
                    ))),
                    provision: None,
                }
            }
            Ok(BotFatherProgress::FloodWait) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotRetryLater,
                    ))),
                    provision: None,
                }
            }
            Ok(BotFatherProgress::Unexpected) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotUnexpected,
                    ))),
                    provision: None,
                }
            }
            Err(crate::setup_telegram::SetupTelegramError::Storage) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotStorageFailed,
                    ))),
                    provision: None,
                }
            }
            Err(crate::setup_telegram::SetupTelegramError::Timeout) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(locale, SetupText::BotTimedOut))),
                    provision: None,
                }
            }
            Err(_) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotVerificationFailed,
                    ))),
                    provision: None,
                }
            }
        }
    }

    pub(super) async fn status(&self, locale: Locale) -> Response {
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        match tokio::task::spawn_blocking(move || {
            SetupStore::new(state_path, token_path).load_state()
        })
        .await
        {
            Ok(Ok(state)) => setup_status_response(
                locale,
                &state.status,
                state.identities.bot_username.as_deref(),
            ),
            Ok(Err(crate::error::SetupStoreError::NotFound)) => {
                Response::plain(setup_text(locale, SetupText::StatusIdle))
            }
            Ok(Err(_)) | Err(_) => {
                Response::plain(setup_text(locale, SetupText::StatusUnavailable))
            }
        }
    }

    pub(super) async fn has_created_bot(&self) -> bool {
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        matches!(
            tokio::task::spawn_blocking(move || SetupStore::new(state_path, token_path).load_state()).await,
            Ok(Ok(state))
                if state.identities.bot_username.is_some()
                    && state.identities.bot_user_id.is_some()
        )
    }

    pub(super) async fn repair(&self, client: &Client, locale: Locale) -> RuntimeExecution {
        let api = match HttpBotApi::new() {
            Ok(api) => api,
            Err(_) => {
                return Response::plain(setup_text(locale, SetupText::RepairCheckUnavailable))
                    .into();
            }
        };
        self.repair_with_api(client, &api, locale).await
    }

    pub(super) async fn repair_with_api(
        &self,
        client: &Client,
        bot_api: &impl BotApi,
        locale: Locale,
    ) -> RuntimeExecution {
        match self.repair_preflight(bot_api, locale).await {
            Ok(username) => RuntimeExecution {
                response: Response::plain(setup_text(locale, SetupText::RepairStarted)),
                provision: Some(ProvisionRequest::new(
                    client.clone(),
                    self.state_path.clone(),
                    self.token_path.clone(),
                    username,
                )),
                shutdown: None,
                post_edit: None,
                onboarding_page: false,
                media: None,
            },
            Err(response) => response.into(),
        }
    }

    pub(super) async fn repair_preflight(
        &self,
        bot_api: &impl BotApi,
        locale: Locale,
    ) -> Result<String, Response> {
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        let loaded = tokio::task::spawn_blocking({
            let state_path = state_path.clone();
            let token_path = token_path.clone();
            move || {
                let store = SetupStore::new(state_path, token_path);
                Ok::<_, crate::error::SetupStoreError>((store.load_state()?, store.load_token()?))
            }
        })
        .await;
        let (state, token) = match loaded {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(crate::error::SetupStoreError::NotFound)) => {
                return Err(Response::plain(setup_text(locale, SetupText::RepairNoData)));
            }
            Ok(Err(_)) | Err(_) => {
                return Err(Response::plain(setup_text(
                    locale,
                    SetupText::RepairDataUnsafe,
                )));
            }
        };
        let username = match state.identities.bot_username.as_deref() {
            Some(username) if crate::setup::validate_username(username).is_ok() => {
                username.to_owned()
            }
            _ => {
                return Err(Response::plain(setup_text(
                    locale,
                    SetupText::RepairIdentityUnsafe,
                )));
            }
        };
        let persisted_bot_id = state.identities.bot_user_id;
        let identity =
            match tokio::time::timeout(Duration::from_secs(25), bot_api.get_me(&token)).await {
                Ok(Ok(identity)) => identity,
                Ok(Err(_)) | Err(_) => {
                    return Err(Response::plain(setup_text(
                        locale,
                        SetupText::RepairTokenUnsafe,
                    )));
                }
            };
        if !identity.username.eq_ignore_ascii_case(&username) {
            return Err(Response::plain(setup_text(
                locale,
                SetupText::RepairTokenMismatch,
            )));
        }
        if let Some(bot_id) = persisted_bot_id {
            if identity.id != bot_id {
                return Err(Response::plain(setup_text(
                    locale,
                    SetupText::RepairTokenMismatch,
                )));
            }
        } else {
            let state_path = self.state_path.clone();
            let token_path = self.token_path.clone();
            let verified_username = identity.username;
            let verified_id = identity.id;
            let expected_username = username.clone();
            let persisted = tokio::task::spawn_blocking(move || {
                let mut store = SetupStore::new(state_path, token_path);
                let mut current = store.load_state()?;
                if current.identities.bot_username.as_deref() != Some(expected_username.as_str())
                    || current.identities.bot_user_id.is_some()
                {
                    return Err(crate::error::SetupStoreError::Read);
                }
                current.identities.bot_username = Some(verified_username);
                current.identities.bot_user_id = Some(verified_id);
                store.save_state(&current)
            })
            .await;
            if !matches!(persisted, Ok(Ok(()))) {
                return Err(Response::plain(setup_text(
                    locale,
                    SetupText::RepairIdentitySaveFailed,
                )));
            }
        }
        Ok(username)
    }
}

pub(super) fn setup_status_label(locale: Locale, status: &str) -> &'static str {
    let key = match status {
        "idle" => SetupText::StatusIdleValue,
        "bot_validated" => SetupText::StatusBotValidated,
        "complete" => SetupText::StatusComplete,
        "completed_without_folder_capacity" => SetupText::StatusCompletedWithoutFolderCapacity,
        "completed_without_folder_name_conflict" => {
            SetupText::StatusCompletedWithoutFolderNameConflict
        }
        "companion_and_community_configured" => SetupText::StatusCompanionAndCommunityConfigured,
        "companion_configured_community_pending" => {
            SetupText::StatusCompanionConfiguredCommunityPending
        }
        _ => SetupText::StatusUnknown,
    };
    setup_text(locale, key)
}

pub(super) fn setup_status_response(locale: Locale, status: &str, bot: Option<&str>) -> Response {
    Response::plain_with_locale(
        locale,
        setup_text(locale, SetupText::Status)
            .replace("{status}", setup_status_label(locale, status))
            .replace(
                "{bot}",
                bot.unwrap_or(setup_text(locale, SetupText::NotConfigured)),
            ),
    )
}
