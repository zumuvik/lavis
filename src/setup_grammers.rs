//! Raw MTProto companion workspace provisioning.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use grammers_client::{Client, message::InputMessage, tl};

use crate::{
    setup_provision::{
        self, AdminRights, BotIdentity, CommunityIdentity, DialogFolder, DialogPeer,
        FolderCapacity, ForumGroup, ForumTopic, ProvisionFuture, ProvisionResult,
        ProvisionTransport,
    },
    setup_store::{PersistedSetupState, SetupStore},
};

/// The public group description is deliberately fixed: it explains the three
/// private operational topics without exposing a bot token or local path.
pub const COMPANION_GROUP_ABOUT: &str = "Lavis companion workspace";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionError {
    ResolveBot,
    StartBot,
    AppConfig,
    CreateGroup,
    CreatedGroupMissing,
    GroupUnavailable,
    GroupChanged,
    SetGroupPhoto,
    GeneralTopicLookup,
    ForumTopics,
    CreateTopic,
    InviteBot,
    PromoteBot,
    DialogFiltersRead,
    DialogFilterDecode,
    DialogFilterUpdate,
    DialogFilterOrder,
    DialogFilterVerify,
    FolderCapacity,
    FolderNameConflict,
    CommunityResolve,
    CommunityInvalidPeer,
    CommunityJoin,
    CommunityUnavailable,
    Storage,
    Timeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InputIdentity {
    id: i64,
    access_hash: i64,
}

/// Construct the production Grammers transport and enter the sole provisioning
/// state machine. This entry deliberately contains no provisioning decisions.
pub async fn production_provision(
    client: &Client,
    state_path: PathBuf,
    token_path: PathBuf,
    bot_username: &str,
) -> Result<ProvisionResult, ProvisionError> {
    let mut state = load_state(state_path.clone(), token_path.clone()).await?;
    let bot = match (
        state.identities.bot_user_id,
        state.identities.bot_access_hash,
    ) {
        (Some(id), Some(access_hash)) => Some(InputIdentity { id, access_hash }),
        _ => None,
    };
    let transport = grammers_transport(
        client,
        state_path,
        token_path,
        group_from_state(&state),
        bot,
        community_from_state(&state),
    );
    run_state_machine(&transport, &mut state, bot_username).await
}

/// Compatibility name used by the existing Telegram task. This is an alias,
/// rather than a second entry point, so it cannot acquire independent flow.
pub use production_provision as provision;

fn grammers_transport(
    client: &Client,
    state_path: PathBuf,
    token_path: PathBuf,
    group: Option<InputIdentity>,
    bot: Option<InputIdentity>,
    community: Option<InputIdentity>,
) -> GrammersTransport<'_> {
    GrammersTransport::new(client, state_path, token_path, group, bot, community)
}

async fn run_state_machine(
    transport: &impl ProvisionTransport,
    state: &mut PersistedSetupState,
    bot_username: &str,
) -> Result<ProvisionResult, ProvisionError> {
    setup_provision::provision(transport, state, bot_username)
        .await
        .map_err(map_provision_error)
}

struct GrammersTransport<'a> {
    client: &'a Client,
    state_path: PathBuf,
    token_path: PathBuf,
    group: Mutex<Option<InputIdentity>>,
    bot: Mutex<Option<InputIdentity>>,
    community: Mutex<Option<InputIdentity>>,
}

impl<'a> GrammersTransport<'a> {
    fn new(
        client: &'a Client,
        state_path: PathBuf,
        token_path: PathBuf,
        group: Option<InputIdentity>,
        bot: Option<InputIdentity>,
        community: Option<InputIdentity>,
    ) -> Self {
        Self {
            client,
            state_path,
            token_path,
            group: Mutex::new(group),
            bot: Mutex::new(bot),
            community: Mutex::new(community),
        }
    }

    fn group(&self, group: ForumGroup) -> Result<InputIdentity, setup_provision::ProvisionError> {
        self.group
            .lock()
            .map_err(|_| setup_provision::ProvisionError::GroupChanged)?
            .filter(|identity| identity.id == group.id)
            .ok_or(setup_provision::ProvisionError::GroupChanged)
    }
}

impl ProvisionTransport for GrammersTransport<'_> {
    fn save_state<'a>(&'a self, state: &'a PersistedSetupState) -> ProvisionFuture<'a, ()> {
        Box::pin(async move {
            persist(&self.state_path, &self.token_path, state)
                .await
                .map_err(|_| setup_provision::ProvisionError::Storage)
        })
    }

    fn resolve_bot<'a>(&'a self, username: &'a str) -> ProvisionFuture<'a, BotIdentity> {
        Box::pin(async move {
            let bot = resolve_bot(self.client, username)
                .await
                .map_err(|_| setup_provision::ProvisionError::ResolveBot)?;
            *self
                .bot
                .lock()
                .map_err(|_| setup_provision::ProvisionError::ResolveBot)? = Some(bot);
            Ok(BotIdentity {
                id: bot.id,
                access_hash: bot.access_hash,
            })
        })
    }

    fn start_bot<'a>(&'a self, bot: BotIdentity) -> ProvisionFuture<'a, ()> {
        Box::pin(async move {
            let bot = InputIdentity {
                id: bot.id,
                access_hash: bot.access_hash,
            };
            self.client
                .send_message(input_peer_user(bot), InputMessage::new().text("/start"))
                .await
                .map_err(|_| setup_provision::ProvisionError::StartBot)?;
            Ok(())
        })
    }

    fn get_app_config<'a>(&'a self) -> ProvisionFuture<'a, FolderCapacity> {
        Box::pin(async move {
            let premium = match self
                .client
                .invoke(&tl::functions::users::GetUsers {
                    id: vec![tl::enums::InputUser::UserSelf],
                })
                .await
                .map_err(|_| setup_provision::ProvisionError::AppConfig)?
                .pop()
            {
                Some(tl::enums::User::User(user)) => user.premium,
                _ => false,
            };
            let config = self
                .client
                .invoke(&tl::functions::help::GetAppConfig { hash: 0 })
                .await
                .map_err(|_| setup_provision::ProvisionError::AppConfig)?;
            Ok(FolderCapacity {
                maximum: app_config_folder_limit(&config, premium),
                first_valid_id: 2,
            })
        })
    }

    fn get_forum_group<'a>(&'a self, group: ForumGroup) -> ProvisionFuture<'a, Option<ForumGroup>> {
        Box::pin(async move {
            let identity = self.group(group)?;
            let chats = self
                .client
                .invoke(&tl::functions::channels::GetChannels {
                    id: vec![input_channel(identity)],
                })
                .await
                .map_err(|_| setup_provision::ProvisionError::GroupUnavailable)?;
            let available_chats = chats.chats();
            let matching = available_chats.iter().find(
                |chat| matches!(chat, tl::enums::Chat::Channel(channel) if channel.id == group.id),
            );
            match matching {
                Some(tl::enums::Chat::Channel(channel)) if channel.megagroup && channel.forum => {
                    Ok(Some(group))
                }
                Some(_) => Err(setup_provision::ProvisionError::GroupChanged),
                None => Ok(None),
            }
        })
    }

    fn create_forum_group<'a>(&'a self, title: &'a str) -> ProvisionFuture<'a, ForumGroup> {
        Box::pin(async move {
            let updates = self
                .client
                .invoke(&tl::functions::channels::CreateChannel {
                    broadcast: false,
                    megagroup: true,
                    for_import: false,
                    forum: true,
                    title: title.to_owned(),
                    about: COMPANION_GROUP_ABOUT.to_owned(),
                    geo_point: None,
                    address: None,
                    ttl_period: None,
                })
                .await
                .map_err(|_| setup_provision::ProvisionError::CreateGroup)?;
            let identity = extract_created_channel(&updates, title)
                .ok_or(setup_provision::ProvisionError::CreateGroup)?;
            *self
                .group
                .lock()
                .map_err(|_| setup_provision::ProvisionError::CreateGroup)? = Some(identity);
            Ok(ForumGroup {
                id: identity.id,
                access_hash: Some(identity.access_hash),
            })
        })
    }

    fn set_forum_group_photo<'a>(&'a self, group: ForumGroup) -> ProvisionFuture<'a, ()> {
        Box::pin(async move {
            // The companion group avatar ships with the binary (assets/
            // grouplogo.png, tracked in git and included by the Nix source
            // filter). A missing or rejected photo must never abort setup,
            // so every failure path surfaces as SetGroupPhoto for the
            // best-effort caller.
            let bytes: &[u8] = include_bytes!("../assets/grouplogo.png");
            let mut stream = std::io::Cursor::new(bytes.to_vec());
            let uploaded = match self
                .client
                .upload_stream(&mut stream, bytes.len(), "lavis-group.png".to_string())
                .await
            {
                Ok(uploaded) => uploaded,
                Err(error) => {
                    tracing::warn!(
                        event = "setup_group_photo_upload_failed",
                        error = %error,
                        "Could not upload the companion group avatar"
                    );
                    return Err(setup_provision::ProvisionError::SetGroupPhoto);
                }
            };
            let file = uploaded.raw;
            let response = self
                .client
                .invoke(&tl::functions::messages::EditChatPhoto {
                    chat_id: group.id,
                    photo: tl::enums::InputChatPhoto::InputChatUploadedPhoto(
                        tl::types::InputChatUploadedPhoto {
                            file: Some(file),
                            video: None,
                            video_start_ts: None,
                            video_emoji_markup: None,
                        },
                    ),
                })
                .await;
            match response {
                Ok(_) => Ok(()),
                Err(error) => {
                    tracing::warn!(
                        event = "setup_group_photo_edit_failed",
                        error = %error,
                        chat_id = group.id,
                        "Telegram rejected the companion group avatar"
                    );
                    Err(setup_provision::ProvisionError::SetGroupPhoto)
                }
            }
        })
    }

    fn get_forum_topic<'a>(
        &'a self,
        group: ForumGroup,
        title: &'a str,
    ) -> ProvisionFuture<'a, Option<ForumTopic>> {
        Box::pin(async move {
            let peer = input_peer_channel(self.group(group)?);
            let topics = self
                .client
                .invoke(&tl::functions::messages::GetForumTopics {
                    q: Some(title.to_owned()),
                    peer,
                    offset_date: 0,
                    offset_id: 0,
                    offset_topic: 0,
                    limit: 20,
                })
                .await
                .map_err(|_| {
                    if title == "General" {
                        setup_provision::ProvisionError::GeneralTopicLookup
                    } else {
                        setup_provision::ProvisionError::CreateTopic
                    }
                })?;
            Ok(find_topic_id(&topics, title).map(|id| ForumTopic { id }))
        })
    }

    fn create_forum_topic<'a>(
        &'a self,
        group: ForumGroup,
        title: &'a str,
    ) -> ProvisionFuture<'a, ForumTopic> {
        Box::pin(async move {
            let peer = input_peer_channel(self.group(group)?);
            self.client
                .invoke(&tl::functions::messages::CreateForumTopic {
                    title_missing: false,
                    peer: peer.clone(),
                    title: title.to_owned(),
                    icon_color: None,
                    icon_emoji_id: None,
                    random_id: random_id()
                        .map_err(|_| setup_provision::ProvisionError::CreateTopic)?,
                    send_as: None,
                })
                .await
                .map_err(|_| setup_provision::ProvisionError::CreateTopic)?;
            let topics = self
                .client
                .invoke(&tl::functions::messages::GetForumTopics {
                    q: Some(title.to_owned()),
                    peer,
                    offset_date: 0,
                    offset_id: 0,
                    offset_topic: 0,
                    limit: 20,
                })
                .await
                .map_err(|_| setup_provision::ProvisionError::CreateTopic)?;
            find_topic_id(&topics, title)
                .map(|id| ForumTopic { id })
                .ok_or(setup_provision::ProvisionError::CreateTopic)
        })
    }

    fn invite_to_channel<'a>(
        &'a self,
        group: ForumGroup,
        bot_username: &'a str,
    ) -> ProvisionFuture<'a, ()> {
        Box::pin(async move {
            let bot = resolve_bot(self.client, bot_username)
                .await
                .map_err(|_| setup_provision::ProvisionError::InviteBot)?;
            match self
                .client
                .invoke(&tl::functions::channels::InviteToChannel {
                    channel: input_channel(self.group(group)?),
                    users: vec![input_user(bot)],
                })
                .await
            {
                Ok(_) => Ok(()),
                Err(error) if error.is("USER_ALREADY_PARTICIPANT") => Ok(()),
                Err(_) => Err(setup_provision::ProvisionError::InviteBot),
            }
        })
    }

    fn edit_admin<'a>(
        &'a self,
        group: ForumGroup,
        bot_username: &'a str,
        _: AdminRights,
    ) -> ProvisionFuture<'a, ()> {
        Box::pin(async move {
            let bot = resolve_bot(self.client, bot_username)
                .await
                .map_err(|_| setup_provision::ProvisionError::PromoteBot)?;
            self.client
                .invoke(&tl::functions::channels::EditAdmin {
                    channel: input_channel(self.group(group)?),
                    user_id: input_user(bot),
                    admin_rights: tl::enums::ChatAdminRights::Rights(minimum_rights()),
                    rank: None,
                })
                .await
                .map_err(|_| setup_provision::ProvisionError::PromoteBot)?;
            Ok(())
        })
    }

    fn resolve_community<'a>(
        &'a self,
        username: &'a str,
    ) -> ProvisionFuture<'a, CommunityIdentity> {
        Box::pin(async move {
            let community = resolve_community(self.client, username).await?;
            *self
                .community
                .lock()
                .map_err(|_| setup_provision::ProvisionError::CommunityResolve)? =
                Some(community.0);
            Ok(community.1)
        })
    }

    fn get_community<'a>(
        &'a self,
        community: CommunityIdentity,
    ) -> ProvisionFuture<'a, Option<CommunityIdentity>> {
        Box::pin(async move {
            let identity = InputIdentity {
                id: community.id,
                access_hash: community.access_hash,
            };
            let chats = self
                .client
                .invoke(&tl::functions::channels::GetChannels {
                    id: vec![input_channel(identity)],
                })
                .await
                .map_err(|_| setup_provision::ProvisionError::CommunityUnavailable)?;
            let found = chats.chats().iter().find_map(|chat| match chat {
                tl::enums::Chat::Channel(channel) if channel.id == community.id => {
                    Some(CommunityIdentity {
                        id: channel.id,
                        access_hash: channel.access_hash?,
                        public: channel.username.is_some(),
                        megagroup: channel.megagroup,
                    })
                }
                _ => None,
            });
            Ok(found)
        })
    }

    fn join_community<'a>(&'a self, community: CommunityIdentity) -> ProvisionFuture<'a, ()> {
        Box::pin(async move {
            let channel = input_channel(InputIdentity {
                id: community.id,
                access_hash: community.access_hash,
            });
            match self
                .client
                .invoke(&tl::functions::channels::JoinChannel { channel })
                .await
            {
                Ok(_) => Ok(()),
                Err(error) => classify_community_join_error(error.is("USER_ALREADY_PARTICIPANT")),
            }
        })
    }

    fn get_dialog_filters<'a>(&'a self) -> ProvisionFuture<'a, Vec<DialogFolder>> {
        Box::pin(async move {
            let filters = self
                .client
                .invoke(&tl::functions::messages::GetDialogFilters {})
                .await
                .map_err(|error| match error {
                    grammers_mtsender::InvocationError::Deserialize(_) => {
                        setup_provision::ProvisionError::DialogFilterDecode
                    }
                    _ => setup_provision::ProvisionError::DialogFiltersRead,
                })?;
            let tl::enums::messages::DialogFilters::Filters(filters) = filters;
            filters
                .filters
                .into_iter()
                .filter_map(|filter| match filter {
                    tl::enums::DialogFilter::Filter(filter) => Some(
                        dialog_peers(&filter.include_peers).and_then(|included_peers| {
                            dialog_peers(&filter.pinned_peers).map(|pinned_peers| DialogFolder {
                                id: filter.id,
                                title: text_with_entities(&filter.title).to_owned(),
                                regular: true,
                                included_peers,
                                pinned_peers,
                            })
                        }),
                    ),
                    tl::enums::DialogFilter::Chatlist(filter) => Some(Ok(DialogFolder {
                        id: filter.id,
                        title: text_with_entities(&filter.title).to_owned(),
                        regular: false,
                        included_peers: Vec::new(),
                        pinned_peers: Vec::new(),
                    })),
                    tl::enums::DialogFilter::Default => None,
                })
                .collect()
        })
    }

    fn update_dialog_filter<'a>(&'a self, folder: DialogFolder) -> ProvisionFuture<'a, ()> {
        Box::pin(async move {
            let peers = folder
                .included_peers
                .iter()
                .copied()
                .map(input_peer_from_dialog_peer)
                .collect();
            let pinned_peers = folder
                .pinned_peers
                .iter()
                .copied()
                .map(input_peer_from_dialog_peer)
                .collect();
            let filter = tl::types::DialogFilter {
                contacts: false,
                non_contacts: false,
                groups: false,
                broadcasts: false,
                bots: false,
                exclude_muted: false,
                exclude_read: false,
                exclude_archived: false,
                title_noanimate: false,
                id: folder.id,
                title: tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities {
                    text: folder.title,
                    entities: Vec::new(),
                }),
                emoticon: Some("🤖".to_owned()),
                color: None,
                pinned_peers,
                include_peers: peers,
                exclude_peers: Vec::new(),
            };
            self.client
                .invoke(&tl::functions::messages::UpdateDialogFilter {
                    id: filter.id,
                    filter: Some(tl::enums::DialogFilter::Filter(filter)),
                })
                .await
                .map_err(|_| setup_provision::ProvisionError::DialogFilterUpdate)?;
            Ok(())
        })
    }

    fn update_dialog_filters_order<'a>(&'a self, order: Vec<i32>) -> ProvisionFuture<'a, ()> {
        Box::pin(async move {
            self.client
                .invoke(&tl::functions::messages::UpdateDialogFiltersOrder { order })
                .await
                .map_err(|_| setup_provision::ProvisionError::DialogFilterOrder)?;
            Ok(())
        })
    }
}

async fn load_state(
    state_path: PathBuf,
    token_path: PathBuf,
) -> Result<PersistedSetupState, ProvisionError> {
    tokio::task::spawn_blocking(move || {
        let store = SetupStore::new(state_path, token_path);
        match store.load_state() {
            Ok(state) => Ok(state),
            Err(crate::error::SetupStoreError::NotFound) => Ok(PersistedSetupState::default()),
            Err(_) => Err(ProvisionError::Storage),
        }
    })
    .await
    .map_err(|_| ProvisionError::Storage)?
}

async fn persist(
    state_path: &Path,
    token_path: &Path,
    state: &PersistedSetupState,
) -> Result<(), ProvisionError> {
    let state_path = state_path.to_path_buf();
    let token_path = token_path.to_path_buf();
    let state = state.clone();
    tokio::task::spawn_blocking(move || SetupStore::new(state_path, token_path).save_state(&state))
        .await
        .map_err(|_| ProvisionError::Storage)?
        .map_err(|_| ProvisionError::Storage)
}

fn map_provision_error(error: setup_provision::ProvisionError) -> ProvisionError {
    match error {
        setup_provision::ProvisionError::ResolveBot => ProvisionError::ResolveBot,
        setup_provision::ProvisionError::StartBot => ProvisionError::StartBot,
        setup_provision::ProvisionError::AppConfig => ProvisionError::AppConfig,
        setup_provision::ProvisionError::CreateGroup => ProvisionError::CreateGroup,
        setup_provision::ProvisionError::Storage => ProvisionError::Storage,
        setup_provision::ProvisionError::GroupUnavailable => ProvisionError::GroupUnavailable,
        setup_provision::ProvisionError::GroupChanged => ProvisionError::GroupChanged,
        setup_provision::ProvisionError::SetGroupPhoto => ProvisionError::SetGroupPhoto,
        setup_provision::ProvisionError::GeneralTopicLookup => ProvisionError::GeneralTopicLookup,
        setup_provision::ProvisionError::FolderCapacity => ProvisionError::FolderCapacity,
        setup_provision::ProvisionError::FolderNameConflict => ProvisionError::FolderNameConflict,
        setup_provision::ProvisionError::CommunityResolve => ProvisionError::CommunityResolve,
        setup_provision::ProvisionError::CommunityInvalidPeer => {
            ProvisionError::CommunityInvalidPeer
        }
        setup_provision::ProvisionError::CommunityJoin => ProvisionError::CommunityJoin,
        setup_provision::ProvisionError::CommunityUnavailable => {
            ProvisionError::CommunityUnavailable
        }
        setup_provision::ProvisionError::CreateTopic => ProvisionError::CreateTopic,
        setup_provision::ProvisionError::InviteBot => ProvisionError::InviteBot,
        setup_provision::ProvisionError::PromoteBot => ProvisionError::PromoteBot,
        setup_provision::ProvisionError::DialogFiltersRead => ProvisionError::DialogFiltersRead,
        setup_provision::ProvisionError::DialogFilterDecode => ProvisionError::DialogFilterDecode,
        setup_provision::ProvisionError::DialogFilterUpdate => ProvisionError::DialogFilterUpdate,
        setup_provision::ProvisionError::DialogFilterOrder => ProvisionError::DialogFilterOrder,
        setup_provision::ProvisionError::DialogFilterVerify => ProvisionError::DialogFilterVerify,
    }
}

/// Telegram does not guarantee that every client receives every config key.
/// Missing, malformed, or non-positive values deliberately allow no new folder.
fn app_config_folder_limit(config: &tl::enums::help::AppConfig, premium: bool) -> usize {
    let key = if premium {
        "dialog_filters_limit_premium"
    } else {
        "dialog_filters_limit_default"
    };
    let tl::enums::help::AppConfig::Config(config) = config else {
        return 0;
    };
    let tl::enums::Jsonvalue::JsonObject(object) = &config.config else {
        return 0;
    };
    object
        .value
        .iter()
        .find_map(|entry| match entry {
            tl::enums::JsonobjectValue::JsonObjectValue(entry) if entry.key == key => {
                match &entry.value {
                    tl::enums::Jsonvalue::JsonNumber(value)
                        if value.value > 0.0 && value.value.fract() == 0.0 =>
                    {
                        Some(value.value as usize)
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .unwrap_or(0)
}

fn dialog_peers(
    peers: &[tl::enums::InputPeer],
) -> Result<Vec<DialogPeer>, setup_provision::ProvisionError> {
    peers
        .iter()
        .map(|peer| match peer {
            tl::enums::InputPeer::User(peer) => Ok(DialogPeer::User {
                id: peer.user_id,
                access_hash: peer.access_hash,
            }),
            tl::enums::InputPeer::Channel(peer) => Ok(DialogPeer::Channel {
                id: peer.channel_id,
                access_hash: peer.access_hash,
            }),
            tl::enums::InputPeer::Chat(peer) => Ok(DialogPeer::Chat { id: peer.chat_id }),
            tl::enums::InputPeer::Empty
            | tl::enums::InputPeer::PeerSelf
            | tl::enums::InputPeer::UserFromMessage(_)
            | tl::enums::InputPeer::ChannelFromMessage(_) => {
                Err(setup_provision::ProvisionError::DialogFilterDecode)
            }
        })
        .collect()
}

fn input_peer_from_dialog_peer(peer: DialogPeer) -> tl::enums::InputPeer {
    match peer {
        DialogPeer::User { id, access_hash } => {
            tl::enums::InputPeer::User(tl::types::InputPeerUser {
                user_id: id,
                access_hash,
            })
        }
        DialogPeer::Channel { id, access_hash } => {
            tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                channel_id: id,
                access_hash,
            })
        }
        DialogPeer::Chat { id } => {
            tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id: id })
        }
    }
}

fn classify_community_join_error(
    already_participant: bool,
) -> Result<(), setup_provision::ProvisionError> {
    if already_participant {
        Ok(())
    } else {
        Err(setup_provision::ProvisionError::CommunityJoin)
    }
}

async fn resolve_bot(client: &Client, username: &str) -> Result<InputIdentity, ProvisionError> {
    let resolved = client
        .invoke(&tl::functions::contacts::ResolveUsername {
            username: username.to_owned(),
            referer: None,
        })
        .await
        .map_err(|_| ProvisionError::ResolveBot)?;
    let tl::enums::contacts::ResolvedPeer::Peer(resolved) = resolved;
    resolved
        .users
        .into_iter()
        .find_map(|user| match user {
            tl::enums::User::User(user)
                if user
                    .username
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(username)) =>
            {
                user.access_hash.map(|access_hash| InputIdentity {
                    id: user.id,
                    access_hash,
                })
            }
            _ => None,
        })
        .ok_or(ProvisionError::ResolveBot)
}

async fn resolve_community(
    client: &Client,
    username: &str,
) -> Result<(InputIdentity, CommunityIdentity), setup_provision::ProvisionError> {
    let resolved = client
        .invoke(&tl::functions::contacts::ResolveUsername {
            username: username.to_owned(),
            referer: None,
        })
        .await
        .map_err(|_| setup_provision::ProvisionError::CommunityResolve)?;
    let tl::enums::contacts::ResolvedPeer::Peer(resolved) = resolved;
    let channel = resolved.chats.into_iter().find_map(|chat| match chat {
        tl::enums::Chat::Channel(channel)
            if channel
                .username
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(username)) =>
        {
            Some(channel)
        }
        _ => None,
    });
    let channel = channel.ok_or(setup_provision::ProvisionError::CommunityInvalidPeer)?;
    let access_hash = channel
        .access_hash
        .ok_or(setup_provision::ProvisionError::CommunityInvalidPeer)?;
    let identity = InputIdentity {
        id: channel.id,
        access_hash,
    };
    Ok((
        identity,
        CommunityIdentity {
            id: channel.id,
            access_hash,
            public: channel.username.is_some(),
            megagroup: channel.megagroup,
        },
    ))
}

fn extract_created_channel(updates: &tl::enums::Updates, title: &str) -> Option<InputIdentity> {
    updates_chats(updates)?.iter().find_map(|chat| match chat {
        tl::enums::Chat::Channel(channel)
            if channel.title == title && channel.megagroup && channel.forum =>
        {
            channel.access_hash.map(|access_hash| InputIdentity {
                id: channel.id,
                access_hash,
            })
        }
        _ => None,
    })
}

fn updates_chats(updates: &tl::enums::Updates) -> Option<&Vec<tl::enums::Chat>> {
    match updates {
        tl::enums::Updates::Combined(updates) => Some(&updates.chats),
        tl::enums::Updates::Updates(updates) => Some(&updates.chats),
        _ => None,
    }
}

fn group_from_state(state: &PersistedSetupState) -> Option<InputIdentity> {
    Some(InputIdentity {
        id: state.identities.companion_chat_id?,
        access_hash: state.identities.companion_chat_access_hash?,
    })
}

fn community_from_state(state: &PersistedSetupState) -> Option<InputIdentity> {
    Some(InputIdentity {
        id: state.identities.community_chat_id?,
        access_hash: state.identities.community_access_hash?,
    })
}

fn text_with_entities(value: &tl::enums::TextWithEntities) -> &str {
    match value {
        tl::enums::TextWithEntities::Entities(value) => &value.text,
    }
}

fn find_topic_id(topics: &tl::enums::messages::ForumTopics, title: &str) -> Option<i32> {
    let tl::enums::messages::ForumTopics::Topics(topics) = topics;
    topics.topics.iter().find_map(|topic| match topic {
        tl::enums::ForumTopic::Topic(topic) if topic.title == title => Some(topic.id),
        _ => None,
    })
}

fn input_channel(identity: InputIdentity) -> tl::enums::InputChannel {
    tl::enums::InputChannel::Channel(tl::types::InputChannel {
        channel_id: identity.id,
        access_hash: identity.access_hash,
    })
}
/// `messages.editChatPhoto` addresses channels by their marked peer id,
/// while provisioning identities store the raw channel id (the same value
/// `InputPeerChannel.channel_id` expects).
fn marked_channel_peer(raw_channel_id: i64) -> i64 {
    -(1_000_000_000_000 + raw_channel_id)
}

fn input_peer_channel(identity: InputIdentity) -> tl::enums::InputPeer {
    tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
        channel_id: identity.id,
        access_hash: identity.access_hash,
    })
}
fn input_user(identity: InputIdentity) -> tl::enums::InputUser {
    tl::enums::InputUser::User(tl::types::InputUser {
        user_id: identity.id,
        access_hash: identity.access_hash,
    })
}
fn input_peer_user(identity: InputIdentity) -> tl::enums::InputPeer {
    tl::enums::InputPeer::User(tl::types::InputPeerUser {
        user_id: identity.id,
        access_hash: identity.access_hash,
    })
}
fn random_id() -> Result<i64, getrandom::Error> {
    Ok(getrandom::u64()? as i64)
}
fn minimum_rights() -> tl::types::ChatAdminRights {
    tl::types::ChatAdminRights {
        change_info: false,
        post_messages: false,
        edit_messages: false,
        delete_messages: true,
        ban_users: false,
        invite_users: false,
        pin_messages: true,
        add_admins: false,
        anonymous: false,
        manage_call: false,
        other: false,
        manage_topics: true,
        post_stories: false,
        edit_stories: false,
        delete_stories: false,
        manage_direct_messages: false,
        manage_ranks: false,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn companion_group_photo_targets_marked_channel_peer() {
        assert_eq!(marked_channel_peer(123), -1_000_000_000_123);
        assert_eq!(marked_channel_peer(3_542_986_112), -1_003_542_986_112);
    }

    use super::{
        ProvisionError, classify_community_join_error, dialog_peers, input_peer_from_dialog_peer,
        map_provision_error, marked_channel_peer, tl,
    };
    use crate::setup_provision::{DialogPeer, ProvisionError as StateError};

    #[test]
    fn operation_categories_are_not_collapsed_into_dialog_filter_failures() {
        assert_eq!(
            map_provision_error(StateError::ResolveBot),
            ProvisionError::ResolveBot
        );
        assert_eq!(
            map_provision_error(StateError::CreateTopic),
            ProvisionError::CreateTopic
        );
        assert_eq!(
            map_provision_error(StateError::InviteBot),
            ProvisionError::InviteBot
        );
        assert_eq!(
            map_provision_error(StateError::PromoteBot),
            ProvisionError::PromoteBot
        );
        assert_eq!(
            map_provision_error(StateError::DialogFiltersRead),
            ProvisionError::DialogFiltersRead
        );
        assert_eq!(
            map_provision_error(StateError::DialogFilterDecode),
            ProvisionError::DialogFilterDecode
        );
        assert_eq!(
            map_provision_error(StateError::DialogFilterUpdate),
            ProvisionError::DialogFilterUpdate
        );
        assert_eq!(
            map_provision_error(StateError::DialogFilterOrder),
            ProvisionError::DialogFilterOrder
        );
        assert_eq!(
            map_provision_error(StateError::DialogFilterVerify),
            ProvisionError::DialogFilterVerify
        );
    }

    #[test]
    fn dialog_peers_round_trip_without_losing_kind_or_access_hash() {
        let input = vec![
            tl::enums::InputPeer::User(tl::types::InputPeerUser {
                user_id: 11,
                access_hash: 101,
            }),
            tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                channel_id: 22,
                access_hash: 202,
            }),
            tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id: 33 }),
        ];

        let owned = dialog_peers(&input).unwrap();
        assert_eq!(
            owned,
            [
                DialogPeer::User {
                    id: 11,
                    access_hash: 101
                },
                DialogPeer::Channel {
                    id: 22,
                    access_hash: 202
                },
                DialogPeer::Chat { id: 33 }
            ]
        );
        assert_eq!(
            owned
                .into_iter()
                .map(input_peer_from_dialog_peer)
                .collect::<Vec<_>>(),
            input
        );
    }

    #[test]
    fn unsupported_dialog_peer_fails_closed() {
        assert_eq!(
            dialog_peers(&[tl::enums::InputPeer::PeerSelf]),
            Err(StateError::DialogFilterDecode)
        );
    }

    #[test]
    fn dialog_peer_debug_redacts_access_hashes() {
        let peer = DialogPeer::User {
            id: 11,
            access_hash: 987654321,
        };
        let debug = format!("{peer:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("987654321"));
    }

    #[test]
    fn user_already_participant_is_the_only_join_error_treated_as_success() {
        assert_eq!(classify_community_join_error(true), Ok(()));
        assert_eq!(
            classify_community_join_error(false),
            Err(StateError::CommunityJoin)
        );
    }
}
