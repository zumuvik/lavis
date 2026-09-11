//! Event-driven companion setup orchestration.
//!
//! Telegram-specific delivery and the Bot API validation boundary are injected,
//! keeping the sequence deterministic and preventing token-bearing URLs from
//! entering the update loop or diagnostics.

use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};

use crate::{
    bot_api::{BotApi, BotApiError},
    setup::{BotToken, UsernameCandidate, classify_botfather_response},
    setup_provision::{CompletedWithoutFolder, ProvisionResult},
    setup_store::{CompanionToken, PersistedSetupState, SetupStore},
};
use grammers_client::{Client, message::InputMessage};
use grammers_session::types::{PeerId, PeerRef};

pub const DISPLAY_NAME: &str = "Lavis — really your userbot";
pub const PROVISION_TIMEOUT: Duration = Duration::from_secs(90);
pub const BOTFATHER_OPERATION_TIMEOUT: Duration = Duration::from_secs(15);

/// The companion bot avatar ships with the binary (assets/botlogo.png,
/// tracked in git and included by the Nix source filter).
pub const BOT_USERPIC: &[u8] = include_bytes!("../assets/botlogo.png");

/// The only data a detached provisioning task may own. It deliberately does
/// not retain runtime or external-module state.
#[derive(Clone)]
pub struct ProvisionRequest {
    client: Client,
    state_path: PathBuf,
    token_path: PathBuf,
    bot_username: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionOutcome {
    Completed,
    CompletedWithoutFolder(CompletedWithoutFolder),
    CompletedWithoutCommunity(crate::setup_provision::ProvisionError),
    Failed(crate::setup_grammers::ProvisionError),
}

impl ProvisionRequest {
    pub fn new(
        client: Client,
        state_path: PathBuf,
        token_path: PathBuf,
        bot_username: String,
    ) -> Self {
        Self {
            client,
            state_path,
            token_path,
            bot_username,
        }
    }

    pub async fn run(self) -> ProvisionOutcome {
        match tokio::time::timeout(
            PROVISION_TIMEOUT,
            crate::setup_grammers::provision(
                &self.client,
                self.state_path,
                self.token_path,
                &self.bot_username,
            ),
        )
        .await
        {
            Ok(Ok(result)) => provision_outcome(result),
            Ok(Err(error)) => {
                tracing::warn!(event = "companion_provision_failed", error_category = ?error, "Companion provisioning failed");
                ProvisionOutcome::Failed(error)
            }
            Err(_) => {
                tracing::warn!(event = "companion_provision_failed", error_category = ?crate::setup_grammers::ProvisionError::Timeout, "Companion provisioning timed out");
                ProvisionOutcome::Failed(crate::setup_grammers::ProvisionError::Timeout)
            }
        }
    }
}

fn provision_outcome(result: ProvisionResult) -> ProvisionOutcome {
    match result {
        ProvisionResult::Completed => ProvisionOutcome::Completed,
        ProvisionResult::CompletedWithoutFolder(reason) => {
            ProvisionOutcome::CompletedWithoutFolder(reason)
        }
        ProvisionResult::CompletedWithoutCommunity(error) => {
            ProvisionOutcome::CompletedWithoutCommunity(error)
        }
    }
}

pub type SetupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), SetupTelegramError>> + Send + 'a>>;

/// A callback button from a BotFather reply. `data` is the raw callback
/// payload required by `messages.getBotCallbackAnswer`.
#[derive(Clone, Debug)]
pub struct BotFatherButton {
    pub msg_id: i32,
    pub text: String,
    pub data: Vec<u8>,
}

/// MTProto boundary. The production adapter is intentionally responsible for
/// raw grammers calls (channel/forum/topic/admin/folder); orchestration never
/// deals in raw TL fields.
pub trait TelegramSetup: Send + Sync {
    fn send_botfather<'a>(&'a self, text: &'a str) -> SetupFuture<'a>;
    fn press_botfather_button<'a>(&'a self, _msg_id: i32, _data: &'a [u8]) -> SetupFuture<'a> {
        Box::pin(async { Err(SetupTelegramError::Telegram) })
    }
    fn send_botfather_photo<'a>(&'a self, _bytes: &'a [u8]) -> SetupFuture<'a> {
        Box::pin(async { Err(SetupTelegramError::Telegram) })
    }
}

/// Production delivery adapter. `PeerRef` is obtained from
/// `Client::resolve_username("BotFather")` and retains the access hash needed
/// by `Client::send_message`.
#[derive(Clone)]
pub struct GrammersTelegramSetup {
    client: Client,
    botfather: PeerRef,
}

impl GrammersTelegramSetup {
    pub async fn resolve(client: &Client) -> Result<(Self, PeerId), SetupTelegramError> {
        tokio::time::timeout(BOTFATHER_OPERATION_TIMEOUT, async {
            let peer = client
                .resolve_username("BotFather")
                .await
                .map_err(|_| SetupTelegramError::Telegram)?
                .ok_or(SetupTelegramError::Telegram)?;
            let peer_id = peer.id();
            let reference = peer
                .to_ref()
                .await
                .map_err(|_| SetupTelegramError::Telegram)?
                .ok_or(SetupTelegramError::Telegram)?;
            Ok((
                Self {
                    client: client.clone(),
                    botfather: reference,
                },
                peer_id,
            ))
        })
        .await
        .map_err(|_| SetupTelegramError::Timeout)?
    }
}

impl TelegramSetup for GrammersTelegramSetup {
    fn send_botfather<'a>(&'a self, text: &'a str) -> SetupFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(BOTFATHER_OPERATION_TIMEOUT, async {
                self.client
                    .send_message(self.botfather, InputMessage::new().text(text))
                    .await
                    .map(|_| ())
                    .map_err(|_| SetupTelegramError::Telegram)
            })
            .await
            .map_err(|_| SetupTelegramError::Timeout)?
        })
    }

    fn press_botfather_button<'a>(&'a self, msg_id: i32, data: &'a [u8]) -> SetupFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(BOTFATHER_OPERATION_TIMEOUT, async {
                let peer = grammers_client::tl::enums::InputPeer::from(&self.botfather);
                self.client
                    .invoke(
                        &grammers_client::tl::functions::messages::GetBotCallbackAnswer {
                            game: false,
                            peer,
                            msg_id,
                            data: Some(data.to_vec()),
                            password: None,
                        },
                    )
                    .await
                    .map(|_| ())
                    .map_err(|_| SetupTelegramError::Telegram)
            })
            .await
            .map_err(|_| SetupTelegramError::Timeout)?
        })
    }

    fn send_botfather_photo<'a>(&'a self, bytes: &'a [u8]) -> SetupFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(BOTFATHER_OPERATION_TIMEOUT, async {
                let mut stream = std::io::Cursor::new(bytes.to_vec());
                let uploaded = self
                    .client
                    .upload_stream(&mut stream, bytes.len(), "lavis-bot.png".to_string())
                    .await
                    .map_err(|_| SetupTelegramError::Telegram)?;
                let message = InputMessage::new().photo(uploaded);
                self.client
                    .send_message(self.botfather, message)
                    .await
                    .map(|_| ())
                    .map_err(|_| SetupTelegramError::Telegram)
            })
            .await
            .map_err(|_| SetupTelegramError::Timeout)?
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupTelegramError {
    Telegram,
    BotApi(BotApiError),
    Storage,
    UsernameMismatch,
    Timeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BotFatherProgress {
    Pending,
    UsernameOccupied,
    UsernameInvalid,
    LimitReached,
    FloodWait,
    Unexpected,
    ProvisionReady,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Cancel,
    NewBot,
    DisplayName,
    Username,
    Complete,
}

/// A single active BotFather conversation. Each BotFather reply advances exactly
/// one step, so `/newbot`, name, and username are never sent speculatively.
pub struct CompanionSetup {
    username: UsernameCandidate,
    step: Step,
    state_path: PathBuf,
    token_path: PathBuf,
}

impl CompanionSetup {
    pub fn new(username: UsernameCandidate, state_path: PathBuf, token_path: PathBuf) -> Self {
        Self {
            username,
            step: Step::Cancel,
            state_path,
            token_path,
        }
    }

    pub async fn start(&mut self, telegram: &impl TelegramSetup) -> Result<(), SetupTelegramError> {
        // Clearing a stale BotFather flow is best effort. Its reply is not a
        // prerequisite for this new conversation.
        let _ = send_with_timeout(telegram, "/cancel").await;
        self.step = Step::NewBot;
        send_with_timeout(telegram, "/newbot").await
    }

    /// Consume only a BotFather reply from the resolved BotFather peer.
    pub async fn on_botfather_reply(
        &mut self,
        text: &str,
        telegram: &impl TelegramSetup,
        bot_api: &impl BotApi,
    ) -> Result<BotFatherProgress, SetupTelegramError> {
        // These are terminal regardless of which prompt we were waiting for.
        // BotFather can send them late, including after a delayed `/cancel`.
        match classify_botfather_response(text) {
            crate::setup::BotFatherResponse::LimitReached => {
                return Ok(BotFatherProgress::LimitReached);
            }
            crate::setup::BotFatherResponse::FloodWait => {
                return Ok(BotFatherProgress::FloodWait);
            }
            _ => {}
        }
        match self.step {
            Step::Cancel => {
                self.step = Step::NewBot;
                send_with_timeout(telegram, "/newbot").await?;
            }
            Step::NewBot => {
                if !is_display_name_prompt(text) {
                    return Ok(BotFatherProgress::Pending);
                }
                self.step = Step::DisplayName;
                send_with_timeout(telegram, DISPLAY_NAME).await?;
            }
            Step::DisplayName => {
                if !is_username_prompt(text) {
                    return Ok(BotFatherProgress::Pending);
                }
                self.step = Step::Username;
                send_with_timeout(telegram, self.username.display()).await?;
            }
            Step::Username => {
                let response = classify_botfather_response(text);
                let token = match response {
                    crate::setup::BotFatherResponse::Success { token } => token,
                    crate::setup::BotFatherResponse::UsernameOccupied => {
                        return Ok(BotFatherProgress::UsernameOccupied);
                    }
                    crate::setup::BotFatherResponse::UsernameInvalid => {
                        return Ok(BotFatherProgress::UsernameInvalid);
                    }
                    crate::setup::BotFatherResponse::LimitReached => {
                        return Ok(BotFatherProgress::LimitReached);
                    }
                    crate::setup::BotFatherResponse::FloodWait => {
                        return Ok(BotFatherProgress::FloodWait);
                    }
                    crate::setup::BotFatherResponse::Unexpected { .. } => {
                        return Ok(BotFatherProgress::Unexpected);
                    }
                };
                self.validate_and_persist(token, bot_api).await?;
                self.step = Step::Complete;
                return Ok(BotFatherProgress::ProvisionReady);
            }
            Step::Complete => return Ok(BotFatherProgress::ProvisionReady),
        }
        Ok(BotFatherProgress::Pending)
    }

    pub fn username(&self) -> String {
        self.username.normalized().to_owned()
    }

    pub fn provision_request(&self, client: Client) -> ProvisionRequest {
        ProvisionRequest::new(
            client,
            self.state_path.clone(),
            self.token_path.clone(),
            self.username.normalized().to_owned(),
        )
    }

    async fn validate_and_persist(
        &self,
        token: BotToken,
        bot_api: &impl BotApi,
    ) -> Result<(), SetupTelegramError> {
        let token = CompanionToken::new(token.as_str().to_owned())
            .map_err(|_| SetupTelegramError::Storage)?;
        let identity = tokio::time::timeout(Duration::from_secs(25), bot_api.get_me(&token))
            .await
            .map_err(|_| SetupTelegramError::Timeout)?
            .map_err(SetupTelegramError::BotApi)?;
        if !identity
            .username
            .eq_ignore_ascii_case(self.username.normalized())
        {
            return Err(SetupTelegramError::UsernameMismatch);
        }
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        let username = identity.username;
        let bot_id = identity.id;
        tokio::task::spawn_blocking(move || {
            let mut store = SetupStore::new(state_path, token_path);
            let mut state = match store.load_state() {
                Ok(state) => state,
                Err(crate::error::SetupStoreError::NotFound) => PersistedSetupState::default(),
                Err(error) => return Err(error),
            };
            // Persist a verified identity before the credential. A crash can
            // therefore leave only a repairable, unvalidated identity; it can
            // never mark a bot validated without its token being durable.
            state.identities.bot_username = Some(username);
            state.identities.bot_user_id = Some(bot_id);
            state.stages.bot_identity_recorded = true;
            store.save_state(&state)?;
            store.save_token(&token)?;
            state.status = "bot_validated".into();
            state.stages.bot_created = true;
            store.save_state(&state)
        })
        .await
        .map_err(|_| SetupTelegramError::Storage)?
        .map_err(|_| SetupTelegramError::Storage)?;
        Ok(())
    }
}

fn is_display_name_prompt(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    !is_cancel_acknowledgement(&text)
        && (text.contains("how are we going to call")
            || text.contains("name for your bot")
            || text.contains("new bot"))
}

fn is_username_prompt(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    !is_cancel_acknowledgement(&text) && text.contains("username")
}

fn is_cancel_acknowledgement(text: &str) -> bool {
    text.contains("cancelled")
        || text.contains("canceled")
        || text.contains("no active conversation")
}

pub const INLINE_PLACEHOLDER: &str = "Что спросить у Lavis?";
pub const INLINE_DESCRIPTION: &str = "Lavis companion bot";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InlineEnableProgress {
    Pending,
    Enabled,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InlinePrompt {
    Placeholder,
    Description,
    Result,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InlineStep {
    SetInlineSent,
    ChooseBot,
    Prompt(InlinePrompt),
    Done,
}

/// A BotFather conversation that enables inline mode for an existing bot via
/// `/setinline`. It runs only after the companion bot is provisioned, so it
/// never races the `/newbot` machine.
pub struct InlineEnableSetup {
    username: String,
    step: InlineStep,
}

impl InlineEnableSetup {
    pub fn new(username: String) -> Self {
        Self {
            username,
            step: InlineStep::SetInlineSent,
        }
    }

    pub fn username(&self) -> String {
        self.username.clone()
    }

    pub async fn start(&mut self, telegram: &impl TelegramSetup) -> Result<(), SetupTelegramError> {
        // Clearing a stale BotFather flow is best effort. Its reply is not a
        // prerequisite for this new conversation.
        let _ = send_with_timeout(telegram, "/cancel").await;
        self.step = InlineStep::ChooseBot;
        send_with_timeout(telegram, "/setinline").await
    }

    pub async fn on_botfather_reply(
        &mut self,
        text: &str,
        buttons: &[BotFatherButton],
        telegram: &impl TelegramSetup,
    ) -> Result<InlineEnableProgress, SetupTelegramError> {
        if self.step == InlineStep::Done {
            return Ok(InlineEnableProgress::Enabled);
        }
        let lowercase = text.to_ascii_lowercase();
        match self.step {
            InlineStep::SetInlineSent | InlineStep::Done => {}
            InlineStep::ChooseBot => {
                if !buttons.is_empty() {
                    let matched = buttons
                        .iter()
                        .find(|button| button.text.to_ascii_lowercase().contains(&self.username));
                    match matched {
                        Some(button) => {
                            telegram
                                .press_botfather_button(button.msg_id, &button.data)
                                .await?;
                        }
                        None => {
                            self.step = InlineStep::Done;
                            return Ok(InlineEnableProgress::Failed);
                        }
                    }
                } else if is_inline_choose_bot_prompt(&lowercase) {
                    send_with_timeout(telegram, &format!("@{}", self.username)).await?;
                } else {
                    return Ok(InlineEnableProgress::Pending);
                }
                self.step = InlineStep::Prompt(InlinePrompt::Placeholder);
            }
            InlineStep::Prompt(prompt) => {
                if is_inline_success(&lowercase) {
                    self.step = InlineStep::Done;
                    return Ok(InlineEnableProgress::Enabled);
                }
                match prompt {
                    InlinePrompt::Placeholder if lowercase.contains("placeholder") => {
                        send_with_timeout(telegram, INLINE_PLACEHOLDER).await?;
                        self.step = InlineStep::Prompt(InlinePrompt::Description);
                    }
                    InlinePrompt::Description if lowercase.contains("description") => {
                        send_with_timeout(telegram, INLINE_DESCRIPTION).await?;
                        self.step = InlineStep::Prompt(InlinePrompt::Result);
                    }
                    _ => {}
                }
            }
        }
        Ok(InlineEnableProgress::Pending)
    }
}

/// A BotFather conversation that sets the companion bot avatar via
/// `/setuserpic`. It chains after the inline conversation settles so
/// BotFather runs one conversation at a time.
pub struct BotPhotoSetup {
    username: String,
    step: BotPhotoStep,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BotPhotoStep {
    SetUserpicSent,
    ChooseBot,
    PhotoAwait,
    WaitConfirm,
    Done,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BotPhotoProgress {
    Pending,
    Completed,
    Failed,
}

impl BotPhotoSetup {
    pub fn new(username: String) -> Self {
        Self {
            username,
            step: BotPhotoStep::SetUserpicSent,
        }
    }

    pub async fn start(&mut self, telegram: &impl TelegramSetup) -> Result<(), SetupTelegramError> {
        let _ = send_with_timeout(telegram, "/cancel").await;
        self.step = BotPhotoStep::ChooseBot;
        send_with_timeout(telegram, "/setuserpic").await
    }

    pub async fn on_botfather_reply(
        &mut self,
        text: &str,
        buttons: &[BotFatherButton],
        telegram: &impl TelegramSetup,
    ) -> Result<BotPhotoProgress, SetupTelegramError> {
        if self.step == BotPhotoStep::Done {
            return Ok(BotPhotoProgress::Completed);
        }
        let lowercase = text.to_ascii_lowercase();
        match self.step {
            BotPhotoStep::SetUserpicSent | BotPhotoStep::Done => {}
            BotPhotoStep::ChooseBot => {
                if !buttons.is_empty() {
                    let matched = buttons
                        .iter()
                        .find(|button| button.text.to_ascii_lowercase().contains(&self.username));
                    match matched {
                        Some(button) => {
                            telegram
                                .press_botfather_button(button.msg_id, &button.data)
                                .await?;
                        }
                        None => {
                            self.step = BotPhotoStep::Done;
                            return Ok(BotPhotoProgress::Failed);
                        }
                    }
                } else if is_inline_choose_bot_prompt(&lowercase) {
                    send_with_timeout(telegram, &format!("@{}", self.username)).await?;
                } else {
                    return Ok(BotPhotoProgress::Pending);
                }
                self.step = BotPhotoStep::PhotoAwait;
            }
            BotPhotoStep::PhotoAwait => {
                if is_inline_success(&lowercase) {
                    self.step = BotPhotoStep::Done;
                    return Ok(BotPhotoProgress::Completed);
                }
                if is_photo_prompt(&lowercase) {
                    telegram.send_botfather_photo(BOT_USERPIC).await?;
                    self.step = BotPhotoStep::WaitConfirm;
                }
            }
            BotPhotoStep::WaitConfirm => {
                if is_inline_success(&lowercase) {
                    self.step = BotPhotoStep::Done;
                    return Ok(BotPhotoProgress::Completed);
                }
            }
        }
        Ok(BotPhotoProgress::Pending)
    }
}

fn is_photo_prompt(text: &str) -> bool {
    text.contains("send") && (text.contains("photo") || text.contains("picture"))
}

fn is_inline_choose_bot_prompt(text: &str) -> bool {
    text.contains("choose a bot")
}

fn is_inline_success(text: &str) -> bool {
    text.contains("success") || text.contains("enabled")
}

async fn send_with_timeout(
    telegram: &impl TelegramSetup,
    text: &str,
) -> Result<(), SetupTelegramError> {
    send_with_timeout_for(telegram, text, BOTFATHER_OPERATION_TIMEOUT).await
}

async fn send_with_timeout_for(
    telegram: &impl TelegramSetup,
    text: &str,
    timeout: Duration,
) -> Result<(), SetupTelegramError> {
    tokio::time::timeout(timeout, telegram.send_botfather(text))
        .await
        .map_err(|_| SetupTelegramError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot_api::{BotApiFuture, BotIdentity};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    type PressedLog = Arc<Mutex<Vec<(i32, Vec<u8>)>>>;
    type PhotoLog = Arc<Mutex<Vec<()>>>;
    struct TelegramMock(Arc<Mutex<Vec<String>>>, PressedLog, PhotoLog);
    impl TelegramMock {
        fn new(sent: Arc<Mutex<Vec<String>>>) -> Self {
            Self(
                sent,
                Arc::new(Mutex::new(Vec::new())),
                Arc::new(Mutex::new(Vec::new())),
            )
        }
    }
    impl TelegramSetup for TelegramMock {
        fn send_botfather<'a>(&'a self, text: &'a str) -> SetupFuture<'a> {
            self.0.lock().unwrap().push(text.into());
            Box::pin(async { Ok(()) })
        }
        fn press_botfather_button<'a>(&'a self, msg_id: i32, data: &'a [u8]) -> SetupFuture<'a> {
            self.1.lock().unwrap().push((msg_id, data.to_vec()));
            Box::pin(async { Ok(()) })
        }
        fn send_botfather_photo<'a>(&'a self, _: &'a [u8]) -> SetupFuture<'a> {
            self.2.lock().unwrap().push(());
            Box::pin(async { Ok(()) })
        }
    }
    struct BotApiMock;
    impl BotApi for BotApiMock {
        fn get_me<'a>(&'a self, _: &'a CompanionToken) -> BotApiFuture<'a> {
            Box::pin(async {
                Ok(BotIdentity {
                    id: 1,
                    username: "lavis_test_bot".into(),
                })
            })
        }
    }

    struct CancelFailsTelegram(Arc<Mutex<Vec<String>>>);
    impl TelegramSetup for CancelFailsTelegram {
        fn send_botfather<'a>(&'a self, text: &'a str) -> SetupFuture<'a> {
            self.0.lock().unwrap().push(text.into());
            Box::pin(async move {
                if text == "/cancel" {
                    Err(SetupTelegramError::Telegram)
                } else {
                    Ok(())
                }
            })
        }
    }

    #[tokio::test]
    async fn advances_botfather_conversation_only_after_replies() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = TelegramMock::new(sent.clone());
        let path = std::env::temp_dir().join(format!("lavis-setup-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut setup = CompanionSetup::new(
            crate::setup::validate_username("lavis_test_bot").unwrap(),
            path.join("state"),
            path.join("token"),
        );
        setup.start(&telegram).await.unwrap();
        for (prompt, expected) in [
            ("How are we going to call it?", BotFatherProgress::Pending),
            (
                "Good. Now let's choose a username for your bot.",
                BotFatherProgress::Pending,
            ),
            ("ok", BotFatherProgress::Unexpected),
        ] {
            assert_eq!(
                setup
                    .on_botfather_reply(prompt, &telegram, &BotApiMock)
                    .await
                    .unwrap(),
                expected
            );
        }
        assert_eq!(
            *sent.lock().unwrap(),
            vec!["/cancel", "/newbot", DISPLAY_NAME, "lavis_test_bot"]
        );
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn starts_newbot_when_optional_cancel_fails() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = CancelFailsTelegram(sent.clone());
        let mut setup = CompanionSetup::new(
            crate::setup::validate_username("lavis_test_bot").unwrap(),
            PathBuf::new(),
            PathBuf::new(),
        );

        setup.start(&telegram).await.unwrap();

        assert_eq!(*sent.lock().unwrap(), ["/cancel", "/newbot"]);
    }

    #[tokio::test]
    async fn stuck_botfather_operation_times_out() {
        struct Stuck;
        impl TelegramSetup for Stuck {
            fn send_botfather<'a>(&'a self, _: &'a str) -> SetupFuture<'a> {
                Box::pin(std::future::pending())
            }
        }

        assert_eq!(
            send_with_timeout_for(&Stuck, "/newbot", Duration::ZERO).await,
            Err(SetupTelegramError::Timeout)
        );
    }

    #[tokio::test]
    async fn ignores_delayed_cancel_reply_until_the_expected_prompt_arrives() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = TelegramMock::new(sent.clone());
        let mut setup = CompanionSetup::new(
            crate::setup::validate_username("lavis_test_bot").unwrap(),
            PathBuf::new(),
            PathBuf::new(),
        );
        setup.start(&telegram).await.unwrap();

        assert_eq!(
            setup
                .on_botfather_reply("No active conversation to cancel.", &telegram, &BotApiMock)
                .await
                .unwrap(),
            BotFatherProgress::Pending
        );
        assert_eq!(*sent.lock().unwrap(), ["/cancel", "/newbot"]);
        setup
            .on_botfather_reply("How are we going to call it?", &telegram, &BotApiMock)
            .await
            .unwrap();
        assert_eq!(*sent.lock().unwrap(), ["/cancel", "/newbot", DISPLAY_NAME]);
    }

    #[tokio::test]
    async fn bot_photo_conversation_presses_and_uploads() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = TelegramMock::new(sent.clone());
        let mut setup = BotPhotoSetup::new("lavis_test_bot".into());
        setup.start(&telegram).await.unwrap();
        setup
            .on_botfather_reply(
                "Choose a bot to change the profile photo of your bot.",
                &[BotFatherButton {
                    msg_id: 5,
                    text: "🤖 @lavis_test_bot".into(),
                    data: b"cb".to_vec(),
                }],
                &telegram,
            )
            .await
            .unwrap();
        assert_eq!(
            setup
                .on_botfather_reply(
                    "OK. Send me the new profile photo for the bot.",
                    &[],
                    &telegram
                )
                .await
                .unwrap(),
            BotPhotoProgress::Pending
        );
        assert_eq!(
            setup
                .on_botfather_reply("Success! Profile photo updated.", &[], &telegram)
                .await
                .unwrap(),
            BotPhotoProgress::Completed
        );
        assert!(!telegram.2.lock().unwrap().is_empty());
        assert_eq!(*sent.lock().unwrap(), ["/cancel", "/setuserpic"]);
    }

    #[tokio::test]
    async fn bot_photo_without_matching_button_fails() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = TelegramMock::new(sent);
        let mut setup = BotPhotoSetup::new("lavis_test_bot".into());
        setup.start(&telegram).await.unwrap();
        assert_eq!(
            setup
                .on_botfather_reply(
                    "Choose a bot to change the profile photo of your bot.",
                    &[BotFatherButton {
                        msg_id: 5,
                        text: "🤖 @someone_else".into(),
                        data: b"x".to_vec(),
                    }],
                    &telegram,
                )
                .await
                .unwrap(),
            BotPhotoProgress::Failed
        );
    }

    #[tokio::test]
    async fn limit_and_flood_are_terminal_before_any_expected_prompt() {
        let telegram = TelegramMock::new(Arc::new(Mutex::new(Vec::new())));
        for step in [Step::NewBot, Step::DisplayName, Step::Username] {
            for (reply, expected) in [
                ("Too many bots", BotFatherProgress::LimitReached),
                ("Try again later", BotFatherProgress::FloodWait),
            ] {
                let mut setup = CompanionSetup::new(
                    crate::setup::validate_username("lavis_test_bot").unwrap(),
                    PathBuf::new(),
                    PathBuf::new(),
                );
                setup.step = step;
                assert_eq!(
                    setup
                        .on_botfather_reply(reply, &telegram, &BotApiMock)
                        .await
                        .unwrap(),
                    expected
                );
            }
        }
    }

    #[tokio::test]
    async fn validated_identity_is_durable_before_the_validated_stage() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = TelegramMock::new(sent);
        let path =
            std::env::temp_dir().join(format!("lavis-setup-identity-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let state_path = path.join("state");
        let token_path = path.join("token");
        let mut setup = CompanionSetup::new(
            crate::setup::validate_username("lavis_test_bot").unwrap(),
            state_path.clone(),
            token_path.clone(),
        );
        setup.start(&telegram).await.unwrap();
        for prompt in [
            "How are we going to call it?",
            "Now choose a username",
            "123456:abcdefghijklmnopqrstUVWX",
        ] {
            setup
                .on_botfather_reply(prompt, &telegram, &BotApiMock)
                .await
                .unwrap();
        }
        let store = SetupStore::new(state_path, token_path);
        let state = store.load_state().unwrap();
        assert_eq!(
            state.identities.bot_username.as_deref(),
            Some("lavis_test_bot")
        );
        assert_eq!(state.identities.bot_user_id, Some(1));
        assert!(state.stages.bot_identity_recorded);
        assert!(state.stages.bot_created);
        assert_eq!(state.status, "bot_validated");
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn inline_enable_presses_matching_button_and_completes() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = TelegramMock::new(sent.clone());
        let pressed = telegram.1.clone();
        let buttons = [
            BotFatherButton {
                msg_id: 10,
                text: "@someone_else".into(),
                data: b"x".to_vec(),
            },
            BotFatherButton {
                msg_id: 11,
                text: "🤖 @lavis_test_bot".into(),
                data: b"cb1".to_vec(),
            },
        ];
        let mut setup = InlineEnableSetup::new("lavis_test_bot".into());
        setup.start(&telegram).await.unwrap();
        assert_eq!(*sent.lock().unwrap(), ["/cancel", "/setinline"]);
        let mut progress = setup
            .on_botfather_reply("Choose a bot to change inline status.", &buttons, &telegram)
            .await
            .unwrap();
        assert_eq!(progress, InlineEnableProgress::Pending);
        assert_eq!(*pressed.lock().unwrap(), [(11, b"cb1".to_vec())]);
        progress = setup
            .on_botfather_reply("Input inline placeholder for the bot.", &[], &telegram)
            .await
            .unwrap();
        assert_eq!(progress, InlineEnableProgress::Pending);
        assert_eq!(sent.lock().unwrap().last().unwrap(), INLINE_PLACEHOLDER);
        progress = setup
            .on_botfather_reply("Success! Inline mode for the bot enabled.", &[], &telegram)
            .await
            .unwrap();
        assert_eq!(progress, InlineEnableProgress::Enabled);
    }

    #[tokio::test]
    async fn inline_enable_fails_without_matching_button() {
        let telegram = TelegramMock::new(Arc::new(Mutex::new(Vec::new())));
        let buttons = [BotFatherButton {
            msg_id: 10,
            text: "@someone_else".into(),
            data: b"x".to_vec(),
        }];
        let mut setup = InlineEnableSetup::new("lavis_test_bot".into());
        setup.start(&telegram).await.unwrap();
        let progress = setup
            .on_botfather_reply("Choose a bot:", &buttons, &telegram)
            .await
            .unwrap();
        assert_eq!(progress, InlineEnableProgress::Failed);
    }

    #[tokio::test]
    async fn inline_enable_tolerates_missing_description_prompt() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = TelegramMock::new(sent.clone());
        let mut setup = InlineEnableSetup::new("lavis_test_bot".into());
        setup.start(&telegram).await.unwrap();
        assert_eq!(
            setup
                .on_botfather_reply("Choose a bot for inline mode.", &[], &telegram,)
                .await
                .unwrap(),
            InlineEnableProgress::Pending
        );
        assert_eq!(sent.lock().unwrap().last().unwrap(), "@lavis_test_bot");
        assert_eq!(
            setup
                .on_botfather_reply("Input inline placeholder:", &[], &telegram)
                .await
                .unwrap(),
            InlineEnableProgress::Pending
        );
        assert_eq!(sent.lock().unwrap().last().unwrap(), INLINE_PLACEHOLDER);
        assert_eq!(
            setup
                .on_botfather_reply("Success! Enabled.", &[], &telegram)
                .await
                .unwrap(),
            InlineEnableProgress::Enabled
        );
        assert!(
            !sent
                .lock()
                .unwrap()
                .iter()
                .any(|text| text == INLINE_DESCRIPTION)
        );
    }

    #[test]
    fn maps_every_provision_result_without_losing_the_partial_reason() {
        assert_eq!(
            provision_outcome(ProvisionResult::Completed),
            ProvisionOutcome::Completed
        );
        assert_eq!(
            provision_outcome(ProvisionResult::CompletedWithoutFolder(
                CompletedWithoutFolder::Capacity,
            )),
            ProvisionOutcome::CompletedWithoutFolder(CompletedWithoutFolder::Capacity)
        );
        assert_eq!(
            provision_outcome(ProvisionResult::CompletedWithoutFolder(
                CompletedWithoutFolder::NameOrOwnershipConflict,
            )),
            ProvisionOutcome::CompletedWithoutFolder(
                CompletedWithoutFolder::NameOrOwnershipConflict,
            )
        );
        assert_eq!(
            provision_outcome(ProvisionResult::CompletedWithoutCommunity(
                crate::setup_provision::ProvisionError::CommunityJoin,
            )),
            ProvisionOutcome::CompletedWithoutCommunity(
                crate::setup_provision::ProvisionError::CommunityJoin,
            )
        );
    }
}
