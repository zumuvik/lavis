use super::{protocol, v6_registry::V6Method};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::Semaphore;

pub const V6_GLOBAL_CONCURRENCY: usize = 8;
const MAX_PAGE_LIMIT: i32 = 100;
const MAX_SUMMARY_COLLECTIONS: usize = 64;

#[derive(Clone)]
pub struct V6ExecutionContext {
    pub module_id: Arc<str>,
}

pub type V6ExecutorFuture<'a> =
    Pin<Box<dyn Future<Output = Result<V6RpcOutput, V6ExecutorError>> + Send + 'a>>;

pub trait V6TelegramExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        context: V6ExecutionContext,
        method: V6Method,
        params: Box<RawValue>,
    ) -> V6ExecutorFuture<'a>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct V6RpcOutput(serde_json::Value);

impl V6RpcOutput {
    pub fn new(value: serde_json::Value) -> Result<Self, V6ExecutorError> {
        protocol::validate_v6_output(&value).map_err(|_| V6ExecutorError::InvalidResponse)?;
        Ok(Self(value))
    }

    pub fn into_value(self) -> serde_json::Value {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V6ExecutorError {
    InvalidParams(&'static str),
    Rpc {
        code: i32,
        name: String,
        retry_after_seconds: Option<u32>,
    },
    Transport,
    InvalidResponse,
    ShuttingDown,
}

#[derive(Clone)]
pub struct GrammersV6Executor {
    client: grammers_client::Client,
    permits: Arc<Semaphore>,
}

impl GrammersV6Executor {
    pub fn new(client: grammers_client::Client) -> Arc<Self> {
        Arc::new(Self {
            client,
            permits: Arc::new(Semaphore::new(V6_GLOBAL_CONCURRENCY)),
        })
    }

    pub fn with_permits(client: grammers_client::Client, permits: Arc<Semaphore>) -> Arc<Self> {
        Arc::new(Self { client, permits })
    }
}

impl V6TelegramExecutor for GrammersV6Executor {
    fn execute<'a>(
        &'a self,
        _context: V6ExecutionContext,
        method: V6Method,
        params: Box<RawValue>,
    ) -> V6ExecutorFuture<'a> {
        Box::pin(async move {
            let _permit = self
                .permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| V6ExecutorError::ShuttingDown)?;
            match method {
                V6Method::AccountUpdateStatus => self.update_status(params).await,
                V6Method::ContactsGetContacts => self.get_contacts(params).await,
                V6Method::MessagesGetHistory => self.get_history(params).await,
                V6Method::MessagesGetDialogs => self.get_dialogs(params).await,
            }
        })
    }
}

impl GrammersV6Executor {
    async fn update_status(&self, params: Box<RawValue>) -> Result<V6RpcOutput, V6ExecutorError> {
        let params = decode::<UpdateStatusParams>(&params)?;
        let request = grammers_client::tl::functions::account::UpdateStatus {
            offline: params.offline,
        };
        let result = self
            .client
            .invoke(&request)
            .await
            .map_err(map_invocation_error)?;
        V6RpcOutput::new(serde_json::Value::Bool(result))
    }

    async fn get_contacts(&self, params: Box<RawValue>) -> Result<V6RpcOutput, V6ExecutorError> {
        let params = decode::<GetContactsParams>(&params)?;
        let request = grammers_client::tl::functions::contacts::GetContacts {
            hash: parse_decimal_i64(&params.hash)?,
        };
        let response = self
            .client
            .invoke(&request)
            .await
            .map_err(map_invocation_error)?;
        // The adapter deliberately exposes no raw users, chats, or access hashes.
        match response {
            grammers_client::tl::enums::contacts::Contacts::Contacts(response) => contacts_summary(
                response.contacts.len(),
                response.users.len(),
                response.saved_count,
            ),
            grammers_client::tl::enums::contacts::Contacts::NotModified(_) => {
                contacts_summary(0, 0, 0)
            }
        }
    }

    async fn get_history(&self, params: Box<RawValue>) -> Result<V6RpcOutput, V6ExecutorError> {
        let params = decode::<GetHistoryParams>(&params)?;
        require_self_peer(&params.peer)?;
        validate_limit(params.limit)?;
        let request = grammers_client::tl::functions::messages::GetHistory {
            peer: grammers_client::tl::enums::InputPeer::PeerSelf,
            offset_id: params.offset_id,
            offset_date: params.offset_date,
            add_offset: params.add_offset,
            limit: params.limit,
            max_id: params.max_id,
            min_id: params.min_id,
            hash: parse_decimal_i64(&params.hash)?,
        };
        let response = self
            .client
            .invoke(&request)
            .await
            .map_err(map_invocation_error)?;
        match response {
            grammers_client::tl::enums::messages::Messages::Messages(response) => history_summary(
                response.messages.len(),
                response.topics.len(),
                response.chats.len(),
                response.users.len(),
                params.limit,
            ),
            grammers_client::tl::enums::messages::Messages::Slice(response) => history_summary(
                response.messages.len(),
                response.topics.len(),
                response.chats.len(),
                response.users.len(),
                params.limit,
            ),
            grammers_client::tl::enums::messages::Messages::ChannelMessages(response) => {
                history_summary(
                    response.messages.len(),
                    response.topics.len(),
                    response.chats.len(),
                    response.users.len(),
                    params.limit,
                )
            }
            grammers_client::tl::enums::messages::Messages::NotModified(_) => {
                history_summary(0, 0, 0, 0, params.limit)
            }
        }
    }

    async fn get_dialogs(&self, params: Box<RawValue>) -> Result<V6RpcOutput, V6ExecutorError> {
        let params = decode::<DialogsInitialParams>(&params)?;
        validate_limit(params.limit)?;
        let request = grammers_client::tl::functions::messages::GetDialogs {
            exclude_pinned: params.exclude_pinned,
            folder_id: params.folder_id,
            offset_date: 0,
            offset_id: 0,
            offset_peer: grammers_client::tl::enums::InputPeer::Empty,
            limit: params.limit,
            hash: parse_decimal_i64(&params.hash)?,
        };
        let response = self
            .client
            .invoke(&request)
            .await
            .map_err(map_invocation_error)?;
        match response {
            grammers_client::tl::enums::messages::Dialogs::Dialogs(response) => dialogs_summary(
                response.dialogs.len(),
                response.messages.len(),
                response.chats.len(),
                response.users.len(),
                params.limit,
            ),
            grammers_client::tl::enums::messages::Dialogs::Slice(response) => dialogs_summary(
                response.dialogs.len(),
                response.messages.len(),
                response.chats.len(),
                response.users.len(),
                params.limit,
            ),
            grammers_client::tl::enums::messages::Dialogs::NotModified(_) => {
                dialogs_summary(0, 0, 0, 0, params.limit)
            }
        }
    }
}

fn map_invocation_error(error: grammers_client::InvocationError) -> V6ExecutorError {
    match error {
        grammers_client::InvocationError::Rpc(error) => {
            map_rpc_error(error.code, error.name, error.value)
        }
        _ => V6ExecutorError::Transport,
    }
}

fn map_rpc_error(code: i32, name: String, value: Option<u32>) -> V6ExecutorError {
    V6ExecutorError::Rpc {
        code,
        retry_after_seconds: (name == "FLOOD_WAIT").then_some(value).flatten(),
        name,
    }
}

fn decode<T: for<'de> Deserialize<'de>>(value: &RawValue) -> Result<T, V6ExecutorError> {
    protocol::validate_v6_raw_params(value)
        .map_err(|_| V6ExecutorError::InvalidParams("invalid params"))?;
    serde_json::from_str(value.get()).map_err(|_| V6ExecutorError::InvalidParams("invalid params"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateStatusParams {
    offline: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetContactsParams {
    hash: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelfPeer {
    kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetHistoryParams {
    peer: SelfPeer,
    #[serde(default)]
    offset_id: i32,
    #[serde(default)]
    offset_date: i32,
    #[serde(default)]
    add_offset: i32,
    #[serde(default = "default_limit")]
    limit: i32,
    #[serde(default)]
    max_id: i32,
    #[serde(default)]
    min_id: i32,
    hash: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DialogsInitialParams {
    #[serde(default)]
    exclude_pinned: bool,
    #[serde(default)]
    folder_id: Option<i32>,
    #[serde(default = "default_limit")]
    limit: i32,
    hash: String,
}

fn default_limit() -> i32 {
    50
}

fn require_self_peer(peer: &SelfPeer) -> Result<(), V6ExecutorError> {
    if peer.kind == "self" {
        Ok(())
    } else {
        Err(V6ExecutorError::InvalidParams("peer must be self"))
    }
}

fn parse_decimal_i64(value: &str) -> Result<i64, V6ExecutorError> {
    if value.is_empty()
        || value.len() > 20
        || !value
            .bytes()
            .enumerate()
            .all(|(index, byte)| byte.is_ascii_digit() || (index == 0 && byte == b'-'))
    {
        return Err(V6ExecutorError::InvalidParams(
            "hash must be decimal string",
        ));
    }
    value
        .parse()
        .map_err(|_| V6ExecutorError::InvalidParams("hash must fit i64"))
}

fn validate_limit(limit: i32) -> Result<(), V6ExecutorError> {
    (1..=MAX_PAGE_LIMIT)
        .contains(&limit)
        .then_some(())
        .ok_or(V6ExecutorError::InvalidParams("limit out of range"))
}

#[derive(Serialize)]
struct ContactsSummary {
    kind: &'static str,
    contacts_count: usize,
    users_count: usize,
    saved_count: i32,
    truncated: bool,
}

#[derive(Serialize)]
struct HistorySummary {
    kind: &'static str,
    messages_count: usize,
    topics_count: usize,
    chats_count: usize,
    users_count: usize,
    truncated: bool,
}

#[derive(Serialize)]
struct DialogsSummary {
    kind: &'static str,
    dialogs_count: usize,
    messages_count: usize,
    chats_count: usize,
    users_count: usize,
    truncated: bool,
}

fn contacts_summary(
    contacts_count: usize,
    users_count: usize,
    saved_count: i32,
) -> Result<V6RpcOutput, V6ExecutorError> {
    let summary = ContactsSummary {
        kind: "contacts_summary",
        contacts_count,
        users_count,
        saved_count,
        truncated: contacts_count > MAX_SUMMARY_COLLECTIONS,
    };
    V6RpcOutput::new(serde_json::to_value(summary).map_err(|_| V6ExecutorError::InvalidResponse)?)
}

fn history_summary(
    messages_count: usize,
    topics_count: usize,
    chats_count: usize,
    users_count: usize,
    limit: i32,
) -> Result<V6RpcOutput, V6ExecutorError> {
    let summary = HistorySummary {
        kind: "history_summary",
        messages_count,
        topics_count,
        chats_count,
        users_count,
        truncated: messages_count >= limit as usize,
    };
    V6RpcOutput::new(serde_json::to_value(summary).map_err(|_| V6ExecutorError::InvalidResponse)?)
}

fn dialogs_summary(
    dialogs_count: usize,
    messages_count: usize,
    chats_count: usize,
    users_count: usize,
    limit: i32,
) -> Result<V6RpcOutput, V6ExecutorError> {
    let summary = DialogsSummary {
        kind: "dialogs_summary",
        dialogs_count,
        messages_count,
        chats_count,
        users_count,
        truncated: dialogs_count >= limit as usize,
    };
    V6RpcOutput::new(serde_json::to_value(summary).map_err(|_| V6ExecutorError::InvalidResponse)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_params_reject_unknown_peers_and_out_of_range_limits() {
        assert!(matches!(
            decode::<GetHistoryParams>(&raw(r#"{"peer":{"kind":"other"},"hash":"1"}"#)),
            Ok(params) if require_self_peer(&params.peer).is_err()
        ));
        assert!(decode::<UpdateStatusParams>(&raw(r#"{"offline":true,"extra":false}"#)).is_err());
        assert!(default_limit() <= MAX_PAGE_LIMIT);
        assert!(parse_decimal_i64("1").is_ok());
        assert!(parse_decimal_i64("1.0").is_err());
        assert!(
            decode::<GetHistoryParams>(&raw(
                r#"{"peer":{"kind":"self","kind":"other"},"hash":"1"}"#
            ))
            .is_err()
        );
        assert!(matches!(
            decode::<DialogsInitialParams>(&raw(r#"{"limit":101,"hash":"1"}"#)),
            Ok(params) if validate_limit(params.limit).is_err()
        ));
    }

    fn raw(value: &str) -> Box<RawValue> {
        RawValue::from_string(value.to_owned()).unwrap()
    }

    #[test]
    fn output_and_rpc_mapping_are_bounded() {
        assert!(V6RpcOutput::new(serde_json::json!({"ok": true})).is_ok());
        assert!(
            V6RpcOutput::new(serde_json::Value::String(
                "x".repeat(protocol::V6_MAX_JSON_STRING_BYTES + 1)
            ))
            .is_err()
        );
        assert_eq!(
            map_rpc_error(420, "FLOOD_WAIT".to_owned(), Some(4)),
            V6ExecutorError::Rpc {
                code: 420,
                name: "FLOOD_WAIT".to_owned(),
                retry_after_seconds: Some(4),
            }
        );
        assert_eq!(
            contacts_summary(2, 3, 4).unwrap().into_value(),
            serde_json::json!({
                "kind": "contacts_summary",
                "contacts_count": 2,
                "users_count": 3,
                "saved_count": 4,
                "truncated": false,
            })
        );
    }
}
