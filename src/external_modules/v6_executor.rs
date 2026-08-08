use super::{protocol, v6_registry::V6Method};
use crate::client::ModuleRpcClient;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::Semaphore;

pub const V6_GLOBAL_CONCURRENCY: usize = 8;
const MAX_PAGE_LIMIT: i32 = 100;
const MAX_SUMMARY_COLLECTIONS: usize = 64;

// V6 currently uses bounded JSON-lines IPC. Keep raw TL bodies below the line
// limit after base64 expansion; large transfers can be split into multiple
// Telegram RPCs without extending Lavis' method surface.
const MAX_RAW_TL_BODY_BYTES: usize = 40 * 1024;
const RAW_BASE64_CHUNK_CHARS: usize = 7 * 1024;
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

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

    fn raw(value: serde_json::Value) -> Result<Self, V6ExecutorError> {
        // Raw replies may exceed MAX_RESULT_BYTES, but they still have to fit
        // the bounded V6 JSON-line transport and per-string JSON guards.
        let encoded = serde_json::to_vec(&value).map_err(|_| V6ExecutorError::InvalidResponse)?;
        if encoded.len() > protocol::MAX_LINE_BYTES.saturating_sub(1024) {
            return Err(V6ExecutorError::InvalidResponse);
        }
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
    Timeout,
    InvalidResponse,
    ShuttingDown,
}

#[derive(Clone)]
pub struct GrammersV6Executor {
    client: grammers_client::Client,
    raw_handle: grammers_mtsender::SenderPoolFatHandle,
    permits: Arc<Semaphore>,
}

impl GrammersV6Executor {
    pub fn new(module_rpc: ModuleRpcClient) -> Arc<Self> {
        Arc::new(Self {
            client: module_rpc.client,
            raw_handle: module_rpc.raw_handle,
            permits: Arc::new(Semaphore::new(V6_GLOBAL_CONCURRENCY)),
        })
    }

    pub fn with_permits(module_rpc: ModuleRpcClient, permits: Arc<Semaphore>) -> Arc<Self> {
        Arc::new(Self {
            client: module_rpc.client,
            raw_handle: module_rpc.raw_handle,
            permits,
        })
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
                V6Method::RawInvoke => self.raw_invoke(params).await,
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
        // The curated adapter deliberately exposes no raw users, chats, or access hashes.
        match response {
            grammers_client::tl::enums::contacts::Contacts::Contacts(response) => contacts_summary(
                response.contacts.len(),
                response.users.len(),
                response.saved_count,
            ),
            grammers_client::tl::enums::contacts::Contacts::NotModified => not_modified_result(),
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
            grammers_client::tl::enums::messages::Messages::NotModified(_) => not_modified_result(),
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
            grammers_client::tl::enums::messages::Dialogs::NotModified(_) => not_modified_result(),
        }
    }

    async fn raw_invoke(&self, params: Box<RawValue>) -> Result<V6RpcOutput, V6ExecutorError> {
        let params = decode::<RawInvokeParams>(&params)?;
        let body = decode_raw_tl_body(&params.body_base64_chunks)?;
        let dc_id = match params.dc_id {
            Some(dc_id) if (1..=100).contains(&dc_id) => dc_id,
            Some(_) => return Err(V6ExecutorError::InvalidParams("dc_id out of range")),
            None => self
                .raw_handle
                .session
                .home_dc_id()
                .map_err(|_| V6ExecutorError::Transport)?,
        };
        let response = self
            .raw_handle
            .invoke_in_dc(dc_id, body)
            .await
            .map_err(map_invocation_error)?;
        raw_tl_result(dc_id, response)
    }
}

fn map_invocation_error(error: grammers_mtsender::InvocationError) -> V6ExecutorError {
    match error {
        grammers_mtsender::InvocationError::Rpc(error) => {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInvokeParams {
    #[serde(default)]
    dc_id: Option<i32>,
    body_base64_chunks: Vec<String>,
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

fn decode_raw_tl_body(chunks: &[String]) -> Result<Vec<u8>, V6ExecutorError> {
    if chunks.is_empty() {
        return Err(V6ExecutorError::InvalidParams("raw body is empty"));
    }
    let encoded_len = chunks.iter().try_fold(0usize, |total, chunk| {
        if chunk.is_empty() || chunk.len() > RAW_BASE64_CHUNK_CHARS {
            return None;
        }
        total.checked_add(chunk.len())
    });
    let Some(encoded_len) = encoded_len else {
        return Err(V6ExecutorError::InvalidParams("invalid raw body chunks"));
    };
    if encoded_len > MAX_RAW_TL_BODY_BYTES.div_ceil(3) * 4 {
        return Err(V6ExecutorError::InvalidParams("raw body is too large"));
    }

    let mut encoded = String::with_capacity(encoded_len);
    for chunk in chunks {
        encoded.push_str(chunk);
    }
    let body = decode_base64(&encoded)?;
    if body.len() < 4 || body.len() > MAX_RAW_TL_BODY_BYTES || body.len() % 4 != 0 {
        return Err(V6ExecutorError::InvalidParams(
            "raw TL body must be 4-byte aligned",
        ));
    }
    Ok(body)
}

fn raw_tl_result(dc_id: i32, body: Vec<u8>) -> Result<V6RpcOutput, V6ExecutorError> {
    if body.len() > MAX_RAW_TL_BODY_BYTES {
        return Err(V6ExecutorError::InvalidResponse);
    }
    let encoded = encode_base64(&body);
    let chunks = encoded
        .as_bytes()
        .chunks(RAW_BASE64_CHUNK_CHARS)
        .map(|chunk| {
            String::from_utf8(chunk.to_vec()).map_err(|_| V6ExecutorError::InvalidResponse)
        })
        .collect::<Result<Vec<_>, _>>()?;
    V6RpcOutput::raw(serde_json::json!({
        "kind": "raw_tl",
        "dc_id": dc_id,
        "body_base64_chunks": chunks,
    }))
}

fn encode_base64(input: &[u8]) -> String {
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let value = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        output.push(BASE64_ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        output.push(BASE64_ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        output.push(if chunk.len() >= 2 {
            BASE64_ALPHABET[((value >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() == 3 {
            BASE64_ALPHABET[(value & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn decode_base64(input: &str) -> Result<Vec<u8>, V6ExecutorError> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return Err(V6ExecutorError::InvalidParams("raw body is not base64"));
    }
    let mut output = Vec::with_capacity((bytes.len() / 4) * 3);
    let chunk_count = bytes.len() / 4;
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let last = index + 1 == chunk_count;
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c_padding = chunk[2] == b'=';
        let d_padding = chunk[3] == b'=';
        if !last && d_padding || c_padding && !d_padding {
            return Err(V6ExecutorError::InvalidParams("raw body is not base64"));
        }
        let c = if c_padding {
            0
        } else {
            base64_value(chunk[2])?
        };
        let d = if d_padding {
            0
        } else {
            base64_value(chunk[3])?
        };
        if c_padding && (b & 0x0f != 0) || d_padding && !c_padding && (c & 0x03 != 0) {
            return Err(V6ExecutorError::InvalidParams("raw body is not base64"));
        }
        output.push((a << 2) | (b >> 4));
        if !c_padding {
            output.push((b << 4) | (c >> 2));
        }
        if !d_padding {
            output.push((c << 6) | d);
        }
    }
    Ok(output)
}

fn base64_value(byte: u8) -> Result<u8, V6ExecutorError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(V6ExecutorError::InvalidParams("raw body is not base64")),
    }
}

fn not_modified_result() -> Result<V6RpcOutput, V6ExecutorError> {
    V6RpcOutput::new(serde_json::json!({ "kind": "not_modified" }))
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

    #[test]
    fn raw_tl_body_is_transport_generic_and_bounded() {
        let body = vec![0x78, 0x56, 0x34, 0x12, 1, 0, 0, 0];
        let encoded = encode_base64(&body);
        let chunks = vec![encoded];
        assert_eq!(decode_raw_tl_body(&chunks).unwrap(), body);

        let unaligned = vec![encode_base64(&[1u8, 2, 3, 4, 5])];
        assert!(decode_raw_tl_body(&unaligned).is_err());
        assert!(decode_raw_tl_body(&[]).is_err());
        assert!(decode_base64("AAAA=").is_err());
        assert!(decode_base64("AA=A").is_err());
    }

    #[test]
    fn raw_tl_result_uses_bounded_chunks() {
        let value = raw_tl_result(2, vec![0u8; 16 * 1024]).unwrap().into_value();
        let chunks = value["body_base64_chunks"].as_array().unwrap();
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.as_str().unwrap().len() <= RAW_BASE64_CHUNK_CHARS)
        );
    }

    #[test]
    fn local_base64_codec_round_trips_padding_cases() {
        for input in [
            b"a".as_slice(),
            b"ab".as_slice(),
            b"abc".as_slice(),
            b"abcd".as_slice(),
        ] {
            let encoded = encode_base64(input);
            assert_eq!(decode_base64(&encoded).unwrap(), input);
        }
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
            not_modified_result().unwrap().into_value(),
            serde_json::json!({"kind": "not_modified"})
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
