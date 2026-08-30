//! Idempotent, transport-independent provisioning of the companion workspace.
//!
//! The raw MTProto adapter belongs at this narrow boundary.  The state machine
//! deliberately works with small owned identifiers so it can be tested without
//! a Telegram connection and persist progress after every remote operation.

use std::{collections::BTreeSet, fmt, future::Future, pin::Pin};

use crate::setup_store::PersistedSetupState;

pub const COMPANION_GROUP_TITLE: &str = "Lavis";
pub const COMPANION_TOPIC_TITLES: [&str; 3] = ["General", "Logs", "Backups"];
pub const COMPANION_FOLDER_TITLE: &str = "Lavis";
pub const COMMUNITY_USERNAME: &str = "lavis_userbot";

pub type ProvisionFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ProvisionError>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForumGroup {
    pub id: i64,
    pub access_hash: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForumTopic {
    pub id: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BotIdentity {
    pub id: i64,
    pub access_hash: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommunityIdentity {
    pub id: i64,
    pub access_hash: i64,
    pub public: bool,
    pub megagroup: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum DialogPeer {
    User { id: i64, access_hash: i64 },
    Channel { id: i64, access_hash: i64 },
    Chat { id: i64 },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PeerKey {
    User(i64),
    Channel(i64),
    Chat(i64),
}

impl DialogPeer {
    pub fn id(self) -> i64 {
        match self {
            Self::User { id, .. } | Self::Channel { id, .. } | Self::Chat { id } => id,
        }
    }

    pub fn key(self) -> PeerKey {
        match self {
            Self::User { id, .. } => PeerKey::User(id),
            Self::Channel { id, .. } => PeerKey::Channel(id),
            Self::Chat { id } => PeerKey::Chat(id),
        }
    }
}

impl fmt::Debug for DialogPeer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User { id, .. } => formatter
                .debug_struct("User")
                .field("id", id)
                .field("access_hash", &"[REDACTED]")
                .finish(),
            Self::Channel { id, .. } => formatter
                .debug_struct("Channel")
                .field("id", id)
                .field("access_hash", &"[REDACTED]")
                .finish(),
            Self::Chat { id } => formatter.debug_struct("Chat").field("id", id).finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdminRights {
    pub manage_topics: bool,
    pub delete_messages: bool,
    pub pin_messages: bool,
}

impl AdminRights {
    pub const MINIMUM: Self = Self {
        manage_topics: true,
        delete_messages: true,
        pin_messages: true,
    };
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DialogFolder {
    pub id: i32,
    pub title: String,
    /// Only regular filters can be owned and repaired by Lavis. Shared
    /// chatlists are never rewritten, even when they share the display name.
    pub regular: bool,
    pub included_peers: Vec<DialogPeer>,
    pub pinned_peers: Vec<DialogPeer>,
}

/// Server-derived dialog filter constraints. This module deliberately does not
/// encode Telegram's current limits or the first valid folder ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FolderCapacity {
    pub maximum: usize,
    pub first_valid_id: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FolderPlan {
    Existing { id: i32, folder: DialogFolder },
    Create { id: i32, folder: DialogFolder },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionError {
    ResolveBot,
    StartBot,
    AppConfig,
    CreateGroup,
    Storage,
    GroupUnavailable,
    GroupChanged,
    GeneralTopicLookup,
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
    SetGroupPhoto,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletedWithoutFolder {
    Capacity,
    NameOrOwnershipConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionResult {
    Completed,
    CompletedWithoutFolder(CompletedWithoutFolder),
    CompletedWithoutCommunity(ProvisionError),
}

/// The only transport surface the staged provisioner needs. The production
/// implementation must map these calls directly to CreateChannel,
/// Get/CreateForumTopic, InviteToChannel/EditAdmin, dialog-filter calls, and
/// GetAppConfig; raw peers stay out of the state machine.
pub trait ProvisionTransport: Send + Sync {
    fn save_state<'a>(&'a self, state: &'a PersistedSetupState) -> ProvisionFuture<'a, ()>;
    fn resolve_bot<'a>(&'a self, username: &'a str) -> ProvisionFuture<'a, BotIdentity>;
    fn start_bot<'a>(&'a self, bot: BotIdentity) -> ProvisionFuture<'a, ()>;
    fn get_app_config<'a>(&'a self) -> ProvisionFuture<'a, FolderCapacity>;
    fn get_forum_group<'a>(&'a self, group: ForumGroup) -> ProvisionFuture<'a, Option<ForumGroup>>;
    fn create_forum_group<'a>(&'a self, title: &'a str) -> ProvisionFuture<'a, ForumGroup>;
    /// Best-effort companion group avatar from the bundled assets; runs on
    /// every provision so repair restores a missing or deleted photo.
    fn set_forum_group_photo<'a>(&'a self, group: ForumGroup) -> ProvisionFuture<'a, ()>;
    fn get_forum_topic<'a>(
        &'a self,
        group: ForumGroup,
        title: &'a str,
    ) -> ProvisionFuture<'a, Option<ForumTopic>>;
    fn create_forum_topic<'a>(
        &'a self,
        group: ForumGroup,
        title: &'a str,
    ) -> ProvisionFuture<'a, ForumTopic>;
    fn invite_to_channel<'a>(
        &'a self,
        group: ForumGroup,
        bot_username: &'a str,
    ) -> ProvisionFuture<'a, ()>;
    fn edit_admin<'a>(
        &'a self,
        group: ForumGroup,
        bot_username: &'a str,
        rights: AdminRights,
    ) -> ProvisionFuture<'a, ()>;
    fn resolve_community<'a>(&'a self, username: &'a str)
    -> ProvisionFuture<'a, CommunityIdentity>;
    fn get_community<'a>(
        &'a self,
        community: CommunityIdentity,
    ) -> ProvisionFuture<'a, Option<CommunityIdentity>>;
    fn join_community<'a>(&'a self, community: CommunityIdentity) -> ProvisionFuture<'a, ()>;
    fn get_dialog_filters<'a>(&'a self) -> ProvisionFuture<'a, Vec<DialogFolder>>;
    fn update_dialog_filter<'a>(&'a self, folder: DialogFolder) -> ProvisionFuture<'a, ()>;
    fn update_dialog_filters_order<'a>(&'a self, order: Vec<i32>) -> ProvisionFuture<'a, ()>;
}

/// Returns a deterministic folder action without mutating Telegram state.
pub fn plan_folder(
    filters: &[DialogFolder],
    companion_chat_id: i64,
    companion_bot_id: i64,
    capacity: FolderCapacity,
    recorded_id: Option<i32>,
) -> Result<FolderPlan, ProvisionError> {
    let group_key = PeerKey::Channel(companion_chat_id);
    let bot_key = PeerKey::User(companion_bot_id);
    let named = filters
        .iter()
        .filter(|folder| folder.title == COMPANION_FOLDER_TITLE)
        .collect::<Vec<_>>();
    if let Some(recorded_id) = recorded_id {
        if let Some(folder) = filters.iter().find(|folder| folder.id == recorded_id) {
            if !folder.regular
                || folder.title != COMPANION_FOLDER_TITLE
                || named.iter().any(|candidate| candidate.id != recorded_id)
            {
                return Err(ProvisionError::FolderNameConflict);
            }
            return Ok(FolderPlan::Existing {
                id: folder.id,
                folder: folder.clone(),
            });
        }
        if !named.is_empty() || filters.len() >= capacity.maximum {
            return Err(ProvisionError::FolderNameConflict);
        }
        return Ok(FolderPlan::Create {
            id: recorded_id,
            folder: DialogFolder {
                id: recorded_id,
                title: COMPANION_FOLDER_TITLE.to_owned(),
                regular: true,
                included_peers: Vec::new(),
                pinned_peers: Vec::new(),
            },
        });
    }
    if named.iter().any(|folder| !folder.regular) {
        return Err(ProvisionError::FolderNameConflict);
    }
    let candidates = named
        .into_iter()
        .filter(|folder| {
            folder
                .included_peers
                .iter()
                .any(|peer| peer.key() == group_key)
                && folder
                    .included_peers
                    .iter()
                    .any(|peer| peer.key() == bot_key)
        })
        .collect::<Vec<_>>();
    if candidates.len() == 1 {
        let folder = candidates[0];
        return Ok(FolderPlan::Existing {
            id: folder.id,
            folder: folder.clone(),
        });
    }
    if !candidates.is_empty()
        || filters
            .iter()
            .any(|folder| folder.title == COMPANION_FOLDER_TITLE)
    {
        return Err(ProvisionError::FolderNameConflict);
    }
    if filters.iter().any(|folder| {
        folder
            .included_peers
            .iter()
            .any(|peer| peer.key() == group_key || peer.key() == bot_key)
    }) {
        return Err(ProvisionError::FolderNameConflict);
    }
    if filters.len() >= capacity.maximum {
        return Err(ProvisionError::FolderCapacity);
    }
    let id = (capacity.first_valid_id..)
        .find(|id| filters.iter().all(|folder| folder.id != *id))
        .ok_or(ProvisionError::FolderCapacity)?;
    Ok(FolderPlan::Create {
        id,
        folder: DialogFolder {
            id,
            title: COMPANION_FOLDER_TITLE.to_owned(),
            regular: true,
            included_peers: Vec::new(),
            pinned_peers: Vec::new(),
        },
    })
}

fn normalized_peer_set(peers: &[DialogPeer]) -> Option<BTreeSet<PeerKey>> {
    let set = peers.iter().map(|peer| peer.key()).collect::<BTreeSet<_>>();
    (set.len() == peers.len()).then_some(set)
}

#[derive(Debug, Eq, PartialEq)]
struct FolderVerification {
    matches: bool,
    included_count: usize,
    pinned_count: usize,
    missing_effective_membership: BTreeSet<PeerKey>,
    missing_pinned: BTreeSet<PeerKey>,
}

fn missing_expected_peers(
    actual: &[DialogPeer],
    expected: &BTreeSet<PeerKey>,
) -> BTreeSet<PeerKey> {
    let actual = actual
        .iter()
        .map(|peer| peer.key())
        .collect::<BTreeSet<_>>();
    expected.difference(&actual).copied().collect()
}

fn verify_folder(
    filters: &[DialogFolder],
    folder_id: i32,
    managed: &[DialogPeer],
) -> FolderVerification {
    let expected = normalized_peer_set(managed).unwrap_or_default();
    let Some(folder) = filters.iter().find(|folder| folder.id == folder_id) else {
        return FolderVerification {
            matches: false,
            included_count: 0,
            pinned_count: 0,
            missing_effective_membership: expected.clone(),
            missing_pinned: expected,
        };
    };
    let effective_membership = folder
        .included_peers
        .iter()
        .chain(&folder.pinned_peers)
        .map(|peer| peer.key())
        .collect::<BTreeSet<_>>();
    let missing_effective_membership = expected
        .difference(&effective_membership)
        .copied()
        .collect::<BTreeSet<_>>();
    let missing_pinned = missing_expected_peers(&folder.pinned_peers, &expected);
    FolderVerification {
        matches: folder.title == COMPANION_FOLDER_TITLE
            && folder.regular
            && normalized_peer_set(&folder.included_peers).is_some()
            && normalized_peer_set(&folder.pinned_peers).is_some()
            && missing_effective_membership.is_empty()
            && missing_pinned.is_empty(),
        included_count: folder.included_peers.len(),
        pinned_count: folder.pinned_peers.len(),
        missing_effective_membership,
        missing_pinned,
    }
}

pub async fn provision(
    transport: &impl ProvisionTransport,
    state: &mut PersistedSetupState,
    bot_username: &str,
) -> Result<ProvisionResult, ProvisionError> {
    let bot = if let (Some(id), Some(access_hash)) = (
        state.identities.bot_user_id,
        state.identities.bot_access_hash,
    ) {
        BotIdentity { id, access_hash }
    } else {
        let bot = transport.resolve_bot(bot_username).await?;
        state.identities.bot_username = Some(bot_username.to_owned());
        state.identities.bot_user_id = Some(bot.id);
        state.identities.bot_access_hash = Some(bot.access_hash);
        persist(transport, state).await?;
        bot
    };
    if !state.stages.bot_dialog_initialized {
        transport.start_bot(bot).await?;
        state.stages.bot_dialog_initialized = true;
        persist(transport, state).await?;
    }
    if !state.stages.app_config_checked {
        transport.get_app_config().await?;
        state.stages.app_config_checked = true;
        persist(transport, state).await?;
    }

    let recorded_group = state.identities.companion_chat_id.map(|id| ForumGroup {
        id,
        access_hash: state.identities.companion_chat_access_hash,
    });
    let group = match recorded_group {
        Some(group) => transport
            .get_forum_group(group)
            .await?
            .ok_or(ProvisionError::GroupUnavailable)?,
        None => {
            let group = transport.create_forum_group(COMPANION_GROUP_TITLE).await?;
            state.identities.companion_chat_id = Some(group.id);
            state.identities.companion_chat_access_hash = group.access_hash;
            state.stages.forum_group_created = true;
            persist(transport, state).await?;
            group
        }
    };
    // Best-effort avatar on every provision, so `setup repair` can restore
    // a group photo deleted or predating the asset pipeline.
    if let Err(error) = transport.set_forum_group_photo(group).await {
        tracing::warn!(
            event = "setup_group_photo_failed",
            error = ?error,
            "Companion group kept its current avatar"
        );
    }

    if !state.stages.forum_group_created {
        state.stages.forum_group_created = true;
        persist(transport, state).await?;
    }
    {
        // General is kept as the durable topic identity. The remaining fixed
        // topics are nevertheless always checked before this stage is saved.
        let mut general = None;
        let mut logs = None;
        let mut backups = None;
        for title in COMPANION_TOPIC_TITLES {
            let topic = match transport.get_forum_topic(group, title).await? {
                Some(topic) => topic,
                None if title == "General" => return Err(ProvisionError::GeneralTopicLookup),
                None => transport.create_forum_topic(group, title).await?,
            };
            if title == "General" {
                general = Some(topic.id);
            } else if title == "Logs" {
                logs = Some(topic.id);
            } else {
                backups = Some(topic.id);
            }
        }
        state.identities.companion_topic_id = general;
        state.identities.companion_logs_topic_id = logs;
        state.identities.companion_backups_topic_id = backups;
        state.stages.forum_topic_created = true;
        persist(transport, state).await?;
    }
    if !state.stages.bot_invited {
        transport.invite_to_channel(group, bot_username).await?;
        state.stages.bot_invited = true;
        persist(transport, state).await?;
    }
    if !state.stages.bot_rights_configured {
        transport
            .edit_admin(group, bot_username, AdminRights::MINIMUM)
            .await?;
        state.stages.bot_rights_configured = true;
        persist(transport, state).await?;
    }
    let community_result = onboard_community(transport, state).await;
    {
        tracing::debug!(
            operation_stage = "initial_read",
            folder_id = ?state.identities.companion_folder_id,
            "Read dialog filters for companion folder planning"
        );
        let folder_capacity = transport.get_app_config().await?;
        let filters = transport.get_dialog_filters().await?;
        let plan = match plan_folder(
            &filters,
            group.id,
            bot.id,
            folder_capacity,
            state.identities.companion_folder_id,
        ) {
            Ok(plan) => plan,
            Err(ProvisionError::FolderCapacity) => {
                state.status = "completed_without_folder_capacity".to_owned();
                state.stages.companion_configured = true;
                persist(transport, state).await?;
                return Ok(ProvisionResult::CompletedWithoutFolder(
                    CompletedWithoutFolder::Capacity,
                ));
            }
            Err(ProvisionError::FolderNameConflict) => {
                state.status = "completed_without_folder_name_conflict".to_owned();
                state.stages.companion_configured = true;
                persist(transport, state).await?;
                return Ok(ProvisionResult::CompletedWithoutFolder(
                    CompletedWithoutFolder::NameOrOwnershipConflict,
                ));
            }
            Err(error) => return Err(error),
        };
        let (folder_id, mut folder) = match plan {
            FolderPlan::Existing { id, folder } | FolderPlan::Create { id, folder } => (id, folder),
        };
        state.identities.companion_folder_id = Some(folder_id);
        state.stages.folder_configured = false;
        persist(transport, state).await?;
        let group_access_hash = group
            .access_hash
            .ok_or(ProvisionError::DialogFilterDecode)?;
        let mut managed = vec![
            DialogPeer::Channel {
                id: group.id,
                access_hash: group_access_hash,
            },
            DialogPeer::User {
                id: bot.id,
                access_hash: bot.access_hash,
            },
        ];
        if community_result.is_ok()
            && state.stages.community_joined
            && let (Some(id), Some(access_hash)) = (
                state.identities.community_chat_id,
                state.identities.community_access_hash,
            )
        {
            managed.push(DialogPeer::Channel { id, access_hash });
        }
        managed.sort_unstable_by_key(|peer| peer.key());
        if normalized_peer_set(&managed).is_none() {
            return Err(ProvisionError::DialogFilterDecode);
        }
        merge_peers(&mut folder.included_peers, &managed)?;
        merge_peers(&mut folder.pinned_peers, &managed)?;
        let managed_keys = managed.iter().map(|peer| peer.key()).collect::<Vec<_>>();
        tracing::debug!(
            operation_stage = "update",
            folder_id,
            managed_peer_keys = ?managed_keys,
            included_peer_count = folder.included_peers.len(),
            pinned_peer_count = folder.pinned_peers.len(),
            "Updating companion dialog filter"
        );
        transport.update_dialog_filter(folder).await?;
        let mut order = filters
            .iter()
            .map(|folder| folder.id)
            .filter(|id| *id != folder_id)
            .collect::<Vec<_>>();
        order.push(folder_id);
        tracing::debug!(
            operation_stage = "order",
            folder_id,
            "Updating companion dialog filter order"
        );
        transport.update_dialog_filters_order(order).await?;
        tracing::debug!(
            operation_stage = "verification_read",
            folder_id,
            "Read dialog filters to verify companion folder"
        );
        let verified_filters = transport.get_dialog_filters().await?;
        let verification = verify_folder(&verified_filters, folder_id, &managed);
        tracing::debug!(
            operation_stage = "verification_result",
            folder_id,
            included_peer_count = verification.included_count,
            pinned_peer_count = verification.pinned_count,
            missing_effective_membership_peer_keys = ?verification.missing_effective_membership,
            missing_pinned_peer_keys = ?verification.missing_pinned,
            "Verified companion dialog filter"
        );
        if !verification.matches {
            return Err(ProvisionError::DialogFilterVerify);
        }
        state.stages.folder_configured = true;
        state.stages.companion_configured = true;
        state.status = if community_result.is_ok() {
            "companion_and_community_configured"
        } else {
            "companion_configured_community_pending"
        }
        .to_owned();
        persist(transport, state).await?;
    }
    match community_result {
        Ok(()) => Ok(ProvisionResult::Completed),
        Err(error) => Ok(ProvisionResult::CompletedWithoutCommunity(error)),
    }
}

fn merge_peers(
    existing: &mut Vec<DialogPeer>,
    managed: &[DialogPeer],
) -> Result<(), ProvisionError> {
    if normalized_peer_set(existing).is_none() {
        return Err(ProvisionError::DialogFilterDecode);
    }
    for peer in managed {
        match existing
            .iter()
            .position(|existing| existing.key() == peer.key())
        {
            Some(index) => existing[index] = *peer,
            None => existing.push(*peer),
        }
    }
    existing.sort_unstable_by_key(|peer| peer.key());
    Ok(())
}

async fn onboard_community(
    transport: &impl ProvisionTransport,
    state: &mut PersistedSetupState,
) -> Result<(), ProvisionError> {
    let community = match (
        state.identities.community_chat_id,
        state.identities.community_access_hash,
    ) {
        (Some(id), Some(access_hash)) => {
            let recorded = CommunityIdentity {
                id,
                access_hash,
                public: true,
                megagroup: true,
            };
            transport
                .get_community(recorded)
                .await?
                .ok_or(ProvisionError::CommunityUnavailable)?
        }
        _ => {
            let resolved = transport.resolve_community(COMMUNITY_USERNAME).await?;
            if !resolved.public || !resolved.megagroup {
                return Err(ProvisionError::CommunityInvalidPeer);
            }
            state.identities.community_chat_id = Some(resolved.id);
            state.identities.community_access_hash = Some(resolved.access_hash);
            persist(transport, state).await?;
            resolved
        }
    };
    if !community.public || !community.megagroup {
        return Err(ProvisionError::CommunityInvalidPeer);
    }
    if !state.stages.community_joined {
        transport.join_community(community).await?;
        state.stages.community_joined = true;
        persist(transport, state).await?;
    }
    Ok(())
}

async fn persist(
    transport: &impl ProvisionTransport,
    state: &PersistedSetupState,
) -> Result<(), ProvisionError> {
    transport.save_state(state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Mock {
        calls: Mutex<Vec<String>>,
        filters: Mutex<Vec<DialogFolder>>,
        topic: Option<ForumTopic>,
        start_fails: bool,
        community_fails: bool,
        community_unavailable: bool,
        join_failures: Mutex<usize>,
        update_failures: Mutex<usize>,
        order_failures: Mutex<usize>,
        dialog_filter_read_failure: Mutex<Option<(usize, ProvisionError)>>,
        verification_update_ignored: bool,
        saved_states: Mutex<Vec<PersistedSetupState>>,
    }
    impl Mock {
        fn call(&self, call: &str) {
            self.calls.lock().unwrap().push(call.into());
        }
    }
    impl ProvisionTransport for Mock {
        fn save_state<'a>(&'a self, state: &'a PersistedSetupState) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                self.saved_states.lock().unwrap().push(state.clone());
                Ok(())
            })
        }
        fn resolve_bot<'a>(&'a self, _: &'a str) -> ProvisionFuture<'a, BotIdentity> {
            Box::pin(async move {
                self.call("resolve");
                Ok(BotIdentity {
                    id: 99,
                    access_hash: 1,
                })
            })
        }
        fn start_bot<'a>(&'a self, _: BotIdentity) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                self.call("start");
                if self.start_fails {
                    return Err(ProvisionError::StartBot);
                }
                Ok(())
            })
        }
        fn get_app_config<'a>(&'a self) -> ProvisionFuture<'a, FolderCapacity> {
            Box::pin(async move {
                self.call("config");
                Ok(FolderCapacity {
                    maximum: 100,
                    first_valid_id: 1000,
                })
            })
        }
        fn get_forum_group<'a>(
            &'a self,
            group: ForumGroup,
        ) -> ProvisionFuture<'a, Option<ForumGroup>> {
            Box::pin(async move { Ok(Some(group)) })
        }
        fn create_forum_group<'a>(&'a self, _: &'a str) -> ProvisionFuture<'a, ForumGroup> {
            Box::pin(async move {
                self.call("group");
                Ok(ForumGroup {
                    id: 42,
                    access_hash: Some(1),
                })
            })
        }
        fn set_forum_group_photo<'a>(&'a self, _: ForumGroup) -> ProvisionFuture<'a, ()> {
            Box::pin(async move { Ok(()) })
        }
        fn get_forum_topic<'a>(
            &'a self,
            _: ForumGroup,
            title: &'a str,
        ) -> ProvisionFuture<'a, Option<ForumTopic>> {
            Box::pin(async move {
                self.call("get-topic");
                Ok(if title == "General" {
                    Some(ForumTopic { id: 1 })
                } else {
                    self.topic
                })
            })
        }
        fn create_forum_topic<'a>(
            &'a self,
            _: ForumGroup,
            _: &'a str,
        ) -> ProvisionFuture<'a, ForumTopic> {
            Box::pin(async move {
                self.call("topic");
                Ok(ForumTopic { id: 7 })
            })
        }
        fn invite_to_channel<'a>(&'a self, _: ForumGroup, _: &'a str) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                self.call("invite");
                Ok(())
            })
        }
        fn edit_admin<'a>(
            &'a self,
            _: ForumGroup,
            _: &'a str,
            rights: AdminRights,
        ) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                assert_eq!(rights, AdminRights::MINIMUM);
                self.call("rights");
                Ok(())
            })
        }
        fn resolve_community<'a>(
            &'a self,
            username: &'a str,
        ) -> ProvisionFuture<'a, CommunityIdentity> {
            Box::pin(async move {
                assert_eq!(username, COMMUNITY_USERNAME);
                self.call("resolve-community");
                if self.community_fails {
                    return Err(ProvisionError::CommunityResolve);
                }
                Ok(CommunityIdentity {
                    id: 77,
                    access_hash: 2,
                    public: true,
                    megagroup: true,
                })
            })
        }
        fn get_community<'a>(
            &'a self,
            community: CommunityIdentity,
        ) -> ProvisionFuture<'a, Option<CommunityIdentity>> {
            Box::pin(async move {
                self.call("get-community");
                Ok((!self.community_unavailable).then_some(community))
            })
        }
        fn join_community<'a>(&'a self, community: CommunityIdentity) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                assert_eq!(community.id, 77);
                self.call("join-community");
                let mut failures = self.join_failures.lock().unwrap();
                if *failures > 0 {
                    *failures -= 1;
                    return Err(ProvisionError::CommunityJoin);
                }
                Ok(())
            })
        }
        fn get_dialog_filters<'a>(&'a self) -> ProvisionFuture<'a, Vec<DialogFolder>> {
            Box::pin(async move {
                self.call("filters");
                let read_count = self
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|call| call.as_str() == "filters")
                    .count();
                let read_failure = *self.dialog_filter_read_failure.lock().unwrap();
                if let Some((failure_read, error)) = read_failure
                    && read_count == failure_read
                {
                    return Err(error);
                }
                Ok(self.filters.lock().unwrap().clone())
            })
        }
        fn update_dialog_filter<'a>(&'a self, folder: DialogFolder) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                assert!(folder.included_peers.iter().any(|peer| peer.id() == 42));
                assert!(folder.included_peers.iter().any(|peer| peer.id() == 99));
                let mut failures = self.update_failures.lock().unwrap();
                if *failures > 0 {
                    *failures -= 1;
                    return Err(ProvisionError::DialogFilterUpdate);
                }
                drop(failures);
                if self.verification_update_ignored {
                    self.call("filter");
                    return Ok(());
                }
                self.filters
                    .lock()
                    .unwrap()
                    .retain(|current| current.id != folder.id);
                self.filters.lock().unwrap().push(folder);
                self.call("filter");
                Ok(())
            })
        }
        fn update_dialog_filters_order<'a>(&'a self, order: Vec<i32>) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                assert_eq!(order.len(), 1);
                self.call("order");
                let mut failures = self.order_failures.lock().unwrap();
                if *failures > 0 {
                    *failures -= 1;
                    return Err(ProvisionError::DialogFilterOrder);
                }
                Ok(())
            })
        }
    }
    #[tokio::test]
    async fn progresses_idempotently_and_persists_each_stage() {
        let transport = Mock::default();
        let mut state = PersistedSetupState::default();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        assert_eq!(
            *transport.calls.lock().unwrap(),
            [
                "resolve",
                "start",
                "config",
                "group",
                "get-topic",
                "get-topic",
                "topic",
                "get-topic",
                "topic",
                "invite",
                "rights",
                "resolve-community",
                "join-community",
                "config",
                "filters",
                "filter",
                "order",
                "filters"
            ]
        );
        transport.calls.lock().unwrap().clear();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        let repair_calls = transport.calls.lock().unwrap().clone();
        assert!(!repair_calls.contains(&"resolve".to_owned()));
        assert!(!repair_calls.contains(&"start".to_owned()));
        assert!(!repair_calls.contains(&"invite".to_owned()));
        assert!(!repair_calls.contains(&"rights".to_owned()));
        assert!(repair_calls.contains(&"get-topic".to_owned()));
        assert!(repair_calls.contains(&"filters".to_owned()));
    }

    #[tokio::test]
    async fn start_failure_does_not_stage_bot_dialog_initialization() {
        let transport = Mock {
            start_fails: true,
            ..Mock::default()
        };
        let mut state = PersistedSetupState::default();

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Err(ProvisionError::StartBot)
        );
        assert!(!state.stages.bot_dialog_initialized);
        assert_eq!(*transport.calls.lock().unwrap(), ["resolve", "start"]);
    }

    #[test]
    fn folder_capacity_conflict_and_order_are_deterministic() {
        let group = 42;
        let filters = vec![DialogFolder {
            id: 4,
            title: "Other".into(),
            regular: true,
            included_peers: vec![DialogPeer::Channel {
                id: group,
                access_hash: 1,
            }],
            pinned_peers: vec![],
        }];
        assert_eq!(
            plan_folder(
                &filters,
                group,
                99,
                FolderCapacity {
                    maximum: 10,
                    first_valid_id: 50,
                },
                None,
            ),
            Err(ProvisionError::FolderNameConflict)
        );
        let full = [50, 51]
            .into_iter()
            .map(|id| DialogFolder {
                id,
                title: id.to_string(),
                regular: true,
                included_peers: vec![],
                pinned_peers: vec![],
            })
            .collect::<Vec<_>>();
        assert_eq!(
            plan_folder(
                &full,
                group,
                99,
                FolderCapacity {
                    maximum: full.len(),
                    first_valid_id: 50,
                },
                None,
            ),
            Err(ProvisionError::FolderCapacity)
        );
        let plan = plan_folder(
            &[DialogFolder {
                id: 2,
                title: "Other".into(),
                regular: true,
                included_peers: vec![],
                pinned_peers: vec![],
            }],
            group,
            99,
            FolderCapacity {
                maximum: 100,
                first_valid_id: 50,
            },
            None,
        )
        .unwrap();
        assert!(matches!(plan, FolderPlan::Create { id: 50, .. }));

        let named_chatlist = DialogFolder {
            id: 9,
            title: COMPANION_FOLDER_TITLE.to_owned(),
            regular: false,
            included_peers: vec![],
            pinned_peers: vec![],
        };
        assert_eq!(
            plan_folder(
                &[named_chatlist],
                group,
                99,
                FolderCapacity {
                    maximum: 10,
                    first_valid_id: 2
                },
                Some(9),
            ),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    fn capacity() -> FolderCapacity {
        FolderCapacity {
            maximum: 100,
            first_valid_id: 2,
        }
    }

    fn folder(id: i32, regular: bool, included: Vec<i64>) -> DialogFolder {
        let peers = included
            .into_iter()
            .map(|id| match id {
                42 | 77 => DialogPeer::Channel { id, access_hash: 1 },
                99 => DialogPeer::User { id, access_hash: 1 },
                _ => DialogPeer::Chat { id },
            })
            .collect::<Vec<_>>();
        DialogFolder {
            id,
            title: COMPANION_FOLDER_TITLE.to_owned(),
            regular,
            included_peers: peers.clone(),
            pinned_peers: peers,
        }
    }

    fn peers(ids: &[i64]) -> Vec<DialogPeer> {
        ids.iter()
            .copied()
            .map(|id| DialogPeer::Chat { id })
            .collect()
    }

    #[test]
    fn adopts_unique_regular_lavis_folder_with_both_core_peers() {
        assert!(matches!(
            plan_folder(&[folder(7, true, vec![42, 99])], 42, 99, capacity(), None),
            Ok(FolderPlan::Existing { id: 7, .. })
        ));
    }

    #[test]
    fn rejects_title_only_folder_without_both_managed_peers() {
        assert_eq!(
            plan_folder(&[folder(7, true, vec![42])], 42, 99, capacity(), None),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    #[test]
    fn rejects_shared_chatlist_named_lavis() {
        assert_eq!(
            plan_folder(&[folder(7, false, vec![42, 99])], 42, 99, capacity(), None),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    #[test]
    fn multiple_adoption_candidates_fail_closed() {
        assert_eq!(
            plan_folder(
                &[folder(7, true, vec![42, 99]), folder(8, true, vec![42, 99]),],
                42,
                99,
                capacity(),
                None
            ),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    #[test]
    fn conflicting_recorded_folder_id_is_rejected() {
        assert_eq!(
            plan_folder(
                &[folder(7, true, vec![42, 99])],
                42,
                99,
                capacity(),
                Some(8)
            ),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    #[test]
    fn reversed_peer_order_passes_verification() {
        assert!(
            verify_folder(
                &[DialogFolder {
                    id: 7,
                    title: COMPANION_FOLDER_TITLE.to_owned(),
                    regular: true,
                    included_peers: peers(&[99, 77, 42]),
                    pinned_peers: peers(&[99, 77, 42]),
                }],
                7,
                &peers(&[42, 77, 99]),
            )
            .matches
        );
    }

    #[test]
    fn empty_included_with_all_managed_pinned_passes_verification() {
        assert!(
            verify_folder(
                &[DialogFolder {
                    id: 7,
                    title: COMPANION_FOLDER_TITLE.to_owned(),
                    regular: true,
                    included_peers: vec![],
                    pinned_peers: peers(&[42, 77, 99]),
                }],
                7,
                &peers(&[42, 77, 99]),
            )
            .matches
        );
    }

    #[test]
    fn managed_included_but_unpinned_peer_fails_verification() {
        let verification = verify_folder(
            &[DialogFolder {
                id: 7,
                title: COMPANION_FOLDER_TITLE.to_owned(),
                regular: true,
                included_peers: peers(&[42, 77, 99]),
                pinned_peers: peers(&[42, 77]),
            }],
            7,
            &peers(&[42, 77, 99]),
        );
        assert!(!verification.matches);
        assert_eq!(verification.missing_effective_membership, BTreeSet::new());
        assert_eq!(
            verification.missing_pinned,
            [PeerKey::Chat(99)].into_iter().collect()
        );
    }

    #[test]
    fn unpinned_user_included_coexists_with_managed_pinned_peers() {
        assert!(
            verify_folder(
                &[DialogFolder {
                    id: 7,
                    title: COMPANION_FOLDER_TITLE.to_owned(),
                    regular: true,
                    included_peers: vec![DialogPeer::User {
                        id: 100,
                        access_hash: 1,
                    }],
                    pinned_peers: peers(&[42, 77, 99]),
                }],
                7,
                &peers(&[42, 77, 99]),
            )
            .matches
        );
    }

    #[test]
    fn managed_peer_absent_from_both_fails_effective_membership() {
        let verification = verify_folder(
            &[DialogFolder {
                id: 7,
                title: COMPANION_FOLDER_TITLE.to_owned(),
                regular: true,
                included_peers: peers(&[42]),
                pinned_peers: peers(&[99]),
            }],
            7,
            &peers(&[42, 77, 99]),
        );

        assert!(!verification.matches);
        assert_eq!(
            verification.missing_effective_membership,
            [PeerKey::Chat(77)].into_iter().collect()
        );
    }

    #[test]
    fn duplicate_managed_peer_ids_are_rejected() {
        let duplicate = peers(&[42, 42, 99]);
        assert!(normalized_peer_set(&duplicate).is_none());
    }

    #[test]
    fn same_numeric_id_in_each_peer_namespace_coexists() {
        let peers = [
            DialogPeer::User {
                id: 42,
                access_hash: 1,
            },
            DialogPeer::Channel {
                id: 42,
                access_hash: 2,
            },
            DialogPeer::Chat { id: 42 },
        ];

        assert_eq!(
            normalized_peer_set(&peers),
            Some(
                [PeerKey::User(42), PeerKey::Channel(42), PeerKey::Chat(42),]
                    .into_iter()
                    .collect()
            )
        );
    }

    #[test]
    fn duplicate_user_with_same_id_fails_closed() {
        assert_eq!(
            normalized_peer_set(&[
                DialogPeer::User {
                    id: 42,
                    access_hash: 1,
                },
                DialogPeer::User {
                    id: 42,
                    access_hash: 2,
                },
            ]),
            None
        );
    }

    #[test]
    fn user_with_group_id_does_not_satisfy_workspace_adoption() {
        let folder = DialogFolder {
            id: 7,
            title: COMPANION_FOLDER_TITLE.to_owned(),
            regular: true,
            included_peers: vec![
                DialogPeer::User {
                    id: 42,
                    access_hash: 1,
                },
                DialogPeer::User {
                    id: 99,
                    access_hash: 2,
                },
            ],
            pinned_peers: Vec::new(),
        };

        assert_eq!(
            plan_folder(&[folder], 42, 99, capacity(), None),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    #[test]
    fn channel_with_bot_id_does_not_satisfy_bot_adoption() {
        let folder = DialogFolder {
            id: 7,
            title: COMPANION_FOLDER_TITLE.to_owned(),
            regular: true,
            included_peers: vec![
                DialogPeer::Channel {
                    id: 42,
                    access_hash: 1,
                },
                DialogPeer::Channel {
                    id: 99,
                    access_hash: 2,
                },
            ],
            pinned_peers: Vec::new(),
        };

        assert_eq!(
            plan_folder(&[folder], 42, 99, capacity(), None),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    #[test]
    fn merge_replaces_only_matching_typed_peer() {
        let mut existing = vec![
            DialogPeer::User {
                id: 42,
                access_hash: 1,
            },
            DialogPeer::Channel {
                id: 42,
                access_hash: 2,
            },
            DialogPeer::Chat { id: 42 },
        ];

        merge_peers(
            &mut existing,
            &[DialogPeer::User {
                id: 42,
                access_hash: 9,
            }],
        )
        .unwrap();

        assert_eq!(existing.len(), 3);
        assert!(existing.contains(&DialogPeer::User {
            id: 42,
            access_hash: 9
        }));
        assert!(existing.contains(&DialogPeer::Channel {
            id: 42,
            access_hash: 2
        }));
        assert!(existing.contains(&DialogPeer::Chat { id: 42 }));
    }

    #[tokio::test]
    async fn dialog_filter_operations_preserve_their_error_categories() {
        let mut malformed_peers = peers(&[42, 42]);
        assert_eq!(
            merge_peers(&mut malformed_peers, &peers(&[99])),
            Err(ProvisionError::DialogFilterDecode)
        );

        let initial_read = Mock {
            dialog_filter_read_failure: Mutex::new(Some((1, ProvisionError::DialogFiltersRead))),
            ..Mock::default()
        };
        assert_eq!(
            provision(
                &initial_read,
                &mut PersistedSetupState::default(),
                "lavis_test_bot"
            )
            .await,
            Err(ProvisionError::DialogFiltersRead)
        );

        let update = Mock {
            update_failures: Mutex::new(1),
            ..Mock::default()
        };
        assert_eq!(
            provision(
                &update,
                &mut PersistedSetupState::default(),
                "lavis_test_bot"
            )
            .await,
            Err(ProvisionError::DialogFilterUpdate)
        );

        let order = Mock {
            order_failures: Mutex::new(1),
            ..Mock::default()
        };
        assert_eq!(
            provision(
                &order,
                &mut PersistedSetupState::default(),
                "lavis_test_bot"
            )
            .await,
            Err(ProvisionError::DialogFilterOrder)
        );

        let verification_read = Mock {
            dialog_filter_read_failure: Mutex::new(Some((2, ProvisionError::DialogFiltersRead))),
            ..Mock::default()
        };
        assert_eq!(
            provision(
                &verification_read,
                &mut PersistedSetupState::default(),
                "lavis_test_bot"
            )
            .await,
            Err(ProvisionError::DialogFiltersRead)
        );

        let verification = Mock {
            verification_update_ignored: true,
            ..Mock::default()
        };
        assert_eq!(
            provision(
                &verification,
                &mut PersistedSetupState::default(),
                "lavis_test_bot"
            )
            .await,
            Err(ProvisionError::DialogFilterVerify)
        );
    }

    #[tokio::test]
    async fn persisted_invite_and_admin_stages_skip_only_completed_operations() {
        let transport = Mock::default();
        let mut state = PersistedSetupState::default();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();

        transport.calls.lock().unwrap().clear();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        assert!(
            !transport
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call == "invite")
        );
        assert!(
            !transport
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call == "rights")
        );

        state.stages.bot_invited = false;
        transport.calls.lock().unwrap().clear();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        let calls = transport.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "invite")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "rights")
                .count(),
            0
        );
        assert!(state.stages.bot_invited);
        assert!(
            transport
                .saved_states
                .lock()
                .unwrap()
                .iter()
                .any(|saved| saved.stages.bot_invited)
        );
    }

    #[test]
    fn peer_debug_output_redacts_access_hashes() {
        let diagnostic = format!(
            "{:?}",
            DialogPeer::Channel {
                id: 42,
                access_hash: 987654321,
            }
        );
        assert!(diagnostic.contains("[REDACTED]"));
        assert!(!diagnostic.contains("987654321"));
    }

    #[tokio::test]
    async fn persisted_folder_id_survives_failure_and_repair_reuses_it() {
        let transport = Mock {
            update_failures: Mutex::new(1),
            ..Mock::default()
        };
        let mut state = PersistedSetupState::default();
        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Err(ProvisionError::DialogFilterUpdate)
        );
        assert_eq!(state.identities.companion_folder_id, Some(1000));
        assert!(!state.stages.folder_configured);
        assert!(
            transport
                .saved_states
                .lock()
                .unwrap()
                .iter()
                .any(|saved| saved.identities.companion_folder_id == Some(1000))
        );

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::Completed)
        );
        assert_eq!(state.identities.companion_folder_id, Some(1000));
        assert_eq!(transport.filters.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn repair_adopts_folder_without_creating_a_second_one() {
        let transport = Mock::default();
        transport
            .filters
            .lock()
            .unwrap()
            .push(folder(12, true, vec![42, 99]));
        let mut state = PersistedSetupState::default();
        state.identities.bot_user_id = Some(99);
        state.identities.bot_access_hash = Some(1);
        state.identities.companion_chat_id = Some(42);
        state.identities.companion_chat_access_hash = Some(1);

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::Completed)
        );
        assert_eq!(state.identities.companion_folder_id, Some(12));
        assert_eq!(transport.filters.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn authenticated_user_joins_official_community_once() {
        let transport = Mock::default();
        let mut state = PersistedSetupState::default();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        let calls = transport.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "join-community")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "resolve-community")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn persisted_successful_join_stage_is_idempotent() {
        let transport = Mock::default();
        let mut state = PersistedSetupState::default();
        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::Completed)
        );
        assert!(state.stages.community_joined);
    }

    #[tokio::test]
    async fn failed_community_join_is_excluded_then_repair_adds_it() {
        let transport = Mock {
            join_failures: Mutex::new(1),
            ..Mock::default()
        };
        let mut state = PersistedSetupState::default();

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::CompletedWithoutCommunity(
                ProvisionError::CommunityJoin
            ))
        );
        assert_eq!(state.identities.community_chat_id, Some(77));
        assert!(!state.stages.community_joined);
        assert!(state.stages.companion_configured);
        assert!(state.stages.folder_configured);
        {
            let filters = transport.filters.lock().unwrap();
            assert_eq!(filters.len(), 1);
            assert!(!filters[0].included_peers.iter().any(|peer| peer.id() == 77));
            assert!(!filters[0].pinned_peers.iter().any(|peer| peer.id() == 77));
        }

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::Completed)
        );
        assert!(state.stages.community_joined);
        let filters = transport.filters.lock().unwrap();
        assert_eq!(filters.len(), 1);
        assert!(filters[0].included_peers.iter().any(|peer| peer.id() == 77));
        assert!(filters[0].pinned_peers.iter().any(|peer| peer.id() == 77));
        let calls = transport.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "resolve-community")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "join-community")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn unavailable_recorded_community_remains_a_partial_core_success() {
        let transport = Mock {
            community_unavailable: true,
            ..Mock::default()
        };
        let mut state = PersistedSetupState::default();
        state.identities.community_chat_id = Some(77);
        state.identities.community_access_hash = Some(2);
        state.stages.community_joined = true;

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::CompletedWithoutCommunity(
                ProvisionError::CommunityUnavailable
            ))
        );
        assert!(state.stages.folder_configured);
        let filters = transport.filters.lock().unwrap();
        assert_eq!(filters.len(), 1);
        assert!(!filters[0].included_peers.iter().any(|peer| peer.id() == 77));
    }

    #[tokio::test]
    async fn companion_bot_is_never_invited_to_official_community() {
        let transport = Mock::default();
        let mut state = PersistedSetupState::default();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        let calls = transport.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "invite")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "join-community")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn folder_contains_all_managed_peers_and_preserves_user_peers() {
        let transport = Mock::default();
        transport
            .filters
            .lock()
            .unwrap()
            .push(folder(12, true, vec![42, 99, 555]));
        let mut state = PersistedSetupState::default();
        state.identities.bot_user_id = Some(99);
        state.identities.bot_access_hash = Some(1);
        state.identities.companion_chat_id = Some(42);
        state.identities.companion_chat_access_hash = Some(1);
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        let filters = transport.filters.lock().unwrap();
        assert_eq!(
            normalized_peer_set(&filters[0].included_peers),
            Some(
                [
                    PeerKey::Channel(42),
                    PeerKey::Channel(77),
                    PeerKey::User(99),
                    PeerKey::Chat(555),
                ]
                .into_iter()
                .collect()
            )
        );
        assert_eq!(
            normalized_peer_set(&filters[0].pinned_peers),
            Some(
                [
                    PeerKey::Channel(42),
                    PeerKey::Channel(77),
                    PeerKey::User(99),
                    PeerKey::Chat(555),
                ]
                .into_iter()
                .collect()
            )
        );
    }

    #[tokio::test]
    async fn community_failure_is_partial_and_preserves_core_workspace() {
        let transport = Mock {
            community_fails: true,
            ..Mock::default()
        };
        let mut state = PersistedSetupState::default();
        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::CompletedWithoutCommunity(
                ProvisionError::CommunityResolve
            ))
        );
        assert!(state.stages.companion_configured);
        assert!(state.stages.folder_configured);
        assert!(!state.stages.community_joined);
        assert_eq!(transport.filters.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pr15_pr16_state_migrates_without_reset() {
        let transport = Mock::default();
        transport
            .filters
            .lock()
            .unwrap()
            .push(folder(12, true, vec![42, 99]));
        let mut state = PersistedSetupState::default();
        state.identities.bot_username = Some("lavis_test_bot".into());
        state.identities.bot_user_id = Some(99);
        state.identities.bot_access_hash = Some(1);
        state.identities.companion_chat_id = Some(42);
        state.identities.companion_chat_access_hash = Some(1);
        state.stages.bot_dialog_initialized = true;
        state.stages.app_config_checked = true;
        state.stages.forum_group_created = true;
        state.stages.forum_topic_created = true;
        state.stages.bot_invited = true;
        state.stages.bot_rights_configured = true;

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::Completed)
        );
        assert_eq!(state.identities.companion_folder_id, Some(12));
        assert!(state.stages.companion_configured);
    }
}
