use crate::error::ExternalError;
use serde::Deserialize;
use serde_json::value::RawValue;

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_LINE_BYTES: usize = 64 * 1024;
pub const MAX_RESULT_BYTES: usize = 32 * 1024;
pub const MAX_ERROR_MESSAGE_CHARS: usize = 256;
pub const MAX_LOG_MESSAGE_CHARS: usize = 1024;
pub const MAX_EVENT_ACTIONS: usize = 1;
pub const MAX_REACTIONS_PER_ACTION: usize = 3;
pub const V6_MAX_JSON_DEPTH: usize = 8;
pub const V6_MAX_JSON_STRING_BYTES: usize = 8 * 1024;
pub const V6_MAX_JSON_COLLECTION_ITEMS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomEmojiEntity {
    pub offset_utf16: usize,
    pub length_utf16: usize,
    pub document_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageEventKind {
    Created,
    Edited,
}

impl MessageEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "message.created",
            Self::Edited => "message.edited",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEvent {
    pub event_id: String,
    pub message_ref: String,
    /// Stable, module-scoped identifier for the same Telegram message across
    /// create/edit events. It does not grant access to the underlying peer.
    pub message_key: String,
    /// Optional Telegram-style peer id, exposed only to modules that declare
    /// `message.peer_id`.
    pub peer_id: Option<i64>,
    pub text: String,
    pub outgoing: bool,
    pub entities: Vec<CustomEmojiEntity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReactionSpec {
    Emoji(String),
    CustomEmoji { document_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventAction {
    pub message_ref: String,
    /// The complete desired reaction set. Protocol v4 permits zero to three
    /// reactions; an empty set removes the account's reactions from a message.
    pub reactions: Vec<ReactionSpec>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CoreMessage {
    Initialize {
        request_id: String,
        module_id: String,
    },
    Execute {
        request_id: String,
        command: String,
        arguments: String,
        argument_entities: Vec<CustomEmojiEntity>,
    },
    Health {
        request_id: String,
    },
    Shutdown {
        request_id: String,
    },
    Event {
        request_id: String,
        event: MessageEventKind,
        payload: MessageEvent,
    },
    TelegramResult {
        request_id: String,
        call_id: String,
        result: Result<serde_json::Value, TelegramCallError>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramCallError {
    pub kind: &'static str,
    pub code: Option<i32>,
    pub name: Option<String>,
    pub message: String,
    pub retry_after_seconds: Option<u32>,
}

#[derive(Debug)]
pub enum V6ModuleFrame {
    TelegramInvoke {
        call_id: String,
        method: String,
        params: Box<RawValue>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V6CallError {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum V6CoreFrame {
    TelegramResult {
        call_id: String,
        result: Result<serde_json::Value, V6CallError>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum V6OutboundCoreFrame {
    Initialize {
        request_id: String,
        module_id: String,
    },
    Execute {
        request_id: String,
        command: String,
        arguments: String,
        argument_entities: Vec<CustomEmojiEntity>,
    },
    Event {
        request_id: String,
        event: MessageEventKind,
        payload: MessageEvent,
    },
    Health {
        request_id: String,
    },
    Shutdown {
        request_id: String,
    },
    TelegramResult {
        call_id: String,
        result: Result<serde_json::Value, V6CallError>,
    },
}

impl V6OutboundCoreFrame {
    pub fn serialize(&self) -> Result<String, ExternalError> {
        match self {
            Self::Initialize {
                request_id,
                module_id,
            } => serialize_v6_lifecycle(
                request_id,
                serde_json::json!({
                    "protocol_version": 6,
                    "type": "initialize",
                    "request_id": request_id,
                    "module_id": module_id,
                }),
            ),
            Self::Execute {
                request_id,
                command,
                arguments,
                argument_entities,
            } => serialize_v6_lifecycle(
                request_id,
                serde_json::json!({
                    "protocol_version": 6,
                    "type": "execute",
                    "request_id": request_id,
                    "command": command,
                    "arguments": arguments,
                    "context": {
                        "argument_entities": argument_entities.iter().map(|entity| serde_json::json!({
                            "type": "custom_emoji",
                            "offset_utf16": entity.offset_utf16,
                            "length_utf16": entity.length_utf16,
                            "document_id": entity.document_id,
                        })).collect::<Vec<_>>(),
                    },
                }),
            ),
            Self::Event {
                request_id,
                event,
                payload,
            } => {
                let mut event_payload = serde_json::json!({
                    "event_id": payload.event_id,
                    "message_ref": payload.message_ref,
                    "message_key": payload.message_key,
                    "text": payload.text,
                    "outgoing": payload.outgoing,
                    "entities": payload.entities.iter().map(|entity| serde_json::json!({
                        "type": "custom_emoji",
                        "offset_utf16": entity.offset_utf16,
                        "length_utf16": entity.length_utf16,
                        "document_id": entity.document_id,
                    })).collect::<Vec<_>>(),
                });
                if let Some(peer_id) = payload.peer_id {
                    event_payload["peer_id"] = serde_json::json!(peer_id);
                }
                serialize_v6_lifecycle(
                    request_id,
                    serde_json::json!({
                        "protocol_version": 6,
                        "type": "event",
                        "request_id": request_id,
                        "event": event.as_str(),
                        "payload": event_payload,
                    }),
                )
            }
            Self::Health { request_id } => serialize_v6_lifecycle(
                request_id,
                serde_json::json!({
                    "protocol_version": 6,
                    "type": "health",
                    "request_id": request_id,
                }),
            ),
            Self::Shutdown { request_id } => serialize_v6_lifecycle(
                request_id,
                serde_json::json!({
                    "protocol_version": 6,
                    "type": "shutdown",
                    "request_id": request_id,
                }),
            ),
            Self::TelegramResult { call_id, result } => {
                serialize_v6_core_result(call_id, result.clone())
            }
        }
    }
}

fn serialize_v6_lifecycle(
    request_id: &str,
    value: serde_json::Value,
) -> Result<String, ExternalError> {
    if !is_v6_request_id(request_id) {
        return Err(ExternalError::ProtocolEncode);
    }
    // This value is generated by Lavis, not supplied by an external module.
    // Inbound module JSON keeps the strict depth/string/collection guard; outbound
    // lifecycle frames are constrained by the documented JSON-line transport bound.
    let line = serde_json::to_string(&value).map_err(|_| ExternalError::ProtocolEncode)?;
    if line.len() > MAX_LINE_BYTES {
        return Err(ExternalError::ProtocolEncode);
    }
    Ok(line)
}

#[derive(Debug)]
pub enum V6InboundFrame {
    Initialized {
        request_id: String,
        module_id: String,
    },
    Result {
        request_id: String,
        text: String,
    },
    Error {
        request_id: String,
        code: String,
        message: String,
    },
    Health {
        request_id: String,
    },
    Log {
        request_id: String,
        level: String,
        message: String,
    },
    EventResult {
        request_id: String,
        actions: Vec<EventAction>,
    },
    TelegramInvoke(V6ModuleFrame),
}

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum V6WireFrame {
    #[serde(rename = "initialized")]
    Initialized {
        protocol_version: u32,
        request_id: String,
        module_id: String,
    },
    #[serde(rename = "result")]
    Result {
        protocol_version: u32,
        request_id: String,
        text: String,
    },
    #[serde(rename = "error")]
    Error {
        protocol_version: u32,
        request_id: String,
        code: String,
        message: String,
    },
    #[serde(rename = "health")]
    Health {
        protocol_version: u32,
        request_id: String,
    },
    #[serde(rename = "log")]
    Log {
        protocol_version: u32,
        request_id: String,
        level: String,
        message: String,
    },
    #[serde(rename = "event_result")]
    EventResult {
        protocol_version: u32,
        request_id: String,
        actions: Vec<serde_json::Value>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V6WireInvoke {
    protocol_version: u32,
    #[serde(rename = "type")]
    message_type: String,
    call_id: String,
    method: String,
    params: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V6WireSuccess {
    protocol_version: u32,
    #[serde(rename = "type")]
    message_type: String,
    call_id: String,
    ok: bool,
    result: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V6WireFailure {
    protocol_version: u32,
    #[serde(rename = "type")]
    message_type: String,
    call_id: String,
    ok: bool,
    error: V6WireError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V6WireError {
    kind: String,
    message: String,
}

pub fn parse_v6_inbound_frame(line: &str) -> Result<V6InboundFrame, ExternalError> {
    let value = parse_v6_value(line)?;
    if value.get("type").and_then(serde_json::Value::as_str) == Some("telegram.invoke") {
        let wire: V6WireInvoke =
            serde_json::from_str(line).map_err(|_| ExternalError::ProtocolDecode)?;
        if wire.protocol_version == 6
            && wire.message_type == "telegram.invoke"
            && is_v6_call_id(&wire.call_id)
            && !wire.method.is_empty()
            && validate_v6_raw_params(&wire.params).is_ok()
        {
            return Ok(V6InboundFrame::TelegramInvoke(
                V6ModuleFrame::TelegramInvoke {
                    call_id: wire.call_id,
                    method: wire.method,
                    params: wire.params,
                },
            ));
        }
        return Err(ExternalError::ProtocolDecode);
    }

    let wire: V6WireFrame = parse_v6_wire(line)?;
    match wire {
        V6WireFrame::Initialized {
            protocol_version: 6,
            request_id,
            module_id,
        } if is_v6_request_id(&request_id) => Ok(V6InboundFrame::Initialized {
            request_id,
            module_id,
        }),
        V6WireFrame::Result {
            protocol_version: 6,
            request_id,
            text,
        } if is_v6_request_id(&request_id) => Ok(V6InboundFrame::Result { request_id, text }),
        V6WireFrame::Error {
            protocol_version: 6,
            request_id,
            code,
            message,
        } if is_v6_request_id(&request_id)
            && code.chars().count() <= MAX_ERROR_MESSAGE_CHARS
            && message.chars().count() <= MAX_ERROR_MESSAGE_CHARS =>
        {
            Ok(V6InboundFrame::Error {
                request_id,
                code,
                message,
            })
        }
        V6WireFrame::Health {
            protocol_version: 6,
            request_id,
        } if is_v6_request_id(&request_id) => Ok(V6InboundFrame::Health { request_id }),
        V6WireFrame::Log {
            protocol_version: 6,
            request_id,
            level,
            message,
        } if is_v6_request_id(&request_id)
            && level.chars().count() <= MAX_LOG_MESSAGE_CHARS
            && message.chars().count() <= MAX_LOG_MESSAGE_CHARS =>
        {
            Ok(V6InboundFrame::Log {
                request_id,
                level,
                message,
            })
        }
        V6WireFrame::EventResult {
            protocol_version: 6,
            request_id,
            actions,
        } if is_v6_request_id(&request_id) && actions.len() <= MAX_EVENT_ACTIONS => {
            let actions = actions
                .iter()
                .map(|action| parse_event_action(action, 6))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(V6InboundFrame::EventResult {
                request_id,
                actions,
            })
        }
        _ => Err(ExternalError::ProtocolDecode),
    }
}

pub fn parse_v6_module_frame(line: &str) -> Result<V6ModuleFrame, ExternalError> {
    match parse_v6_inbound_frame(line)? {
        V6InboundFrame::TelegramInvoke(frame) => Ok(frame),
        _ => Err(ExternalError::ProtocolDecode),
    }
}

pub fn parse_v6_core_frame(line: &str) -> Result<V6CoreFrame, ExternalError> {
    let _ = parse_v6_value(line)?;
    let success = serde_json::from_str::<V6WireSuccess>(line);
    if let Ok(success) = success {
        if success.protocol_version == 6
            && success.message_type == "telegram.result"
            && success.ok
            && is_v6_call_id(&success.call_id)
        {
            return Ok(V6CoreFrame::TelegramResult {
                call_id: success.call_id,
                result: Ok(success.result),
            });
        }
        return Err(ExternalError::ProtocolDecode);
    }
    let failure: V6WireFailure =
        serde_json::from_str(line).map_err(|_| ExternalError::ProtocolDecode)?;
    if failure.protocol_version != 6
        || failure.message_type != "telegram.result"
        || failure.ok
        || !is_v6_call_id(&failure.call_id)
        || failure.error.kind.is_empty()
    {
        return Err(ExternalError::ProtocolDecode);
    }
    Ok(V6CoreFrame::TelegramResult {
        call_id: failure.call_id,
        result: Err(V6CallError {
            kind: failure.error.kind,
            message: failure.error.message,
        }),
    })
}

pub fn serialize_v6_core_result(
    call_id: &str,
    result: Result<serde_json::Value, V6CallError>,
) -> Result<String, ExternalError> {
    if !is_v6_call_id(call_id) {
        return Err(ExternalError::ProtocolEncode);
    }
    let value = match result {
        Ok(result) => serde_json::json!({
            "protocol_version": 6,
            "type": "telegram.result",
            "call_id": call_id,
            "ok": true,
            "result": result,
        }),
        Err(error) if !error.kind.is_empty() => serde_json::json!({
            "protocol_version": 6,
            "type": "telegram.result",
            "call_id": call_id,
            "ok": false,
            "error": {"kind": error.kind, "message": error.message},
        }),
        Err(_) => return Err(ExternalError::ProtocolEncode),
    };
    guard_v6_json(&value, 0).map_err(|_| ExternalError::ProtocolEncode)?;
    let line = serde_json::to_string(&value).map_err(|_| ExternalError::ProtocolEncode)?;
    if line.len() > MAX_LINE_BYTES {
        return Err(ExternalError::ProtocolEncode);
    }
    Ok(line)
}

fn parse_v6_value(line: &str) -> Result<serde_json::Value, ExternalError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(ExternalError::LineTooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|_| ExternalError::ProtocolDecode)?;
    guard_v6_json(&value, 0)?;
    Ok(value)
}

fn parse_v6_wire(line: &str) -> Result<V6WireFrame, ExternalError> {
    let _ = parse_v6_value(line)?;
    serde_json::from_str(line).map_err(|_| ExternalError::ProtocolDecode)
}

fn guard_v6_json(value: &serde_json::Value, depth: usize) -> Result<(), ExternalError> {
    if depth > V6_MAX_JSON_DEPTH {
        return Err(ExternalError::ProtocolDecode);
    }
    match value {
        serde_json::Value::String(string) if string.len() > V6_MAX_JSON_STRING_BYTES => {
            Err(ExternalError::ProtocolDecode)
        }
        serde_json::Value::Array(values) => {
            if values.len() > V6_MAX_JSON_COLLECTION_ITEMS {
                return Err(ExternalError::ProtocolDecode);
            }
            for value in values {
                guard_v6_json(value, depth + 1)?;
            }
            Ok(())
        }
        serde_json::Value::Object(values) => {
            if values.len() > V6_MAX_JSON_COLLECTION_ITEMS {
                return Err(ExternalError::ProtocolDecode);
            }
            for (key, value) in values {
                if key.len() > V6_MAX_JSON_STRING_BYTES {
                    return Err(ExternalError::ProtocolDecode);
                }
                guard_v6_json(value, depth + 1)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub(crate) fn validate_v6_output(value: &serde_json::Value) -> Result<(), ExternalError> {
    guard_v6_json(value, 0).map_err(|_| ExternalError::ProtocolEncode)?;
    let encoded = serde_json::to_vec(value).map_err(|_| ExternalError::ProtocolEncode)?;
    if encoded.len() > MAX_RESULT_BYTES {
        return Err(ExternalError::ProtocolEncode);
    }
    Ok(())
}

pub(crate) fn validate_v6_raw_params(params: &RawValue) -> Result<(), ExternalError> {
    let value: serde_json::Value =
        serde_json::from_str(params.get()).map_err(|_| ExternalError::ProtocolDecode)?;
    guard_v6_json(&value, 0)
}

fn is_v6_call_id(call_id: &str) -> bool {
    !call_id.is_empty()
        && call_id.len() <= 64
        && call_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn is_v6_request_id(request_id: &str) -> bool {
    !request_id.is_empty()
        && request_id.len() <= 64
        && request_id.bytes().all(|byte| byte.is_ascii_digit())
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModuleMessage {
    Initialized {
        request_id: String,
        module_id: String,
    },
    Result {
        request_id: String,
        text: String,
    },
    Error {
        request_id: String,
        code: String,
        message: String,
    },
    Health {
        request_id: String,
    },
    Log {
        request_id: String,
        level: String,
        message: String,
    },
    EventResult {
        request_id: String,
        actions: Vec<EventAction>,
    },
    TelegramInvoke {
        request_id: String,
        call_id: String,
        method: String,
        params: serde_json::Value,
    },
}

impl CoreMessage {
    pub fn serialize(&self) -> Result<String, ExternalError> {
        self.serialize_for(PROTOCOL_VERSION)
    }

    pub fn serialize_for(&self, protocol_version: u32) -> Result<String, ExternalError> {
        match self {
            Self::Initialize {
                request_id,
                module_id,
            } => serde_json::to_string(&serde_json::json!({
                "protocol_version": protocol_version,
                "type": "initialize",
                "request_id": request_id,
                "module_id": module_id,
            })),
            Self::Execute {
                request_id,
                command,
                arguments,
                argument_entities,
            } => {
                let mut message = serde_json::json!({
                    "protocol_version": protocol_version,
                    "type": "execute",
                    "request_id": request_id,
                    "command": command,
                    "arguments": arguments,
                });
                if protocol_version >= 3 {
                    message["context"] = serde_json::json!({
                        "argument_entities": argument_entities.iter().map(|entity| serde_json::json!({
                            "type": "custom_emoji",
                            "offset_utf16": entity.offset_utf16,
                            "length_utf16": entity.length_utf16,
                            "document_id": entity.document_id,
                        })).collect::<Vec<_>>(),
                    });
                }
                serde_json::to_string(&message)
            }
            Self::Health { request_id } => serde_json::to_string(&serde_json::json!({
                "protocol_version": protocol_version,
                "type": "health",
                "request_id": request_id,
            })),
            Self::Shutdown { request_id } => serde_json::to_string(&serde_json::json!({
                "protocol_version": protocol_version,
                "type": "shutdown",
                "request_id": request_id,
            })),
            Self::Event {
                request_id,
                event,
                payload,
            } => {
                if protocol_version < 3
                    || (*event == MessageEventKind::Edited && protocol_version < 4)
                {
                    return Err(ExternalError::ProtocolEncode);
                }
                let mut event_payload = serde_json::json!({
                    "event_id": payload.event_id,
                    "message_ref": payload.message_ref,
                    "text": payload.text,
                    "outgoing": payload.outgoing,
                    "entities": payload.entities.iter().map(|entity| serde_json::json!({
                        "type": "custom_emoji",
                        "offset_utf16": entity.offset_utf16,
                        "length_utf16": entity.length_utf16,
                        "document_id": entity.document_id,
                    })).collect::<Vec<_>>(),
                });
                if protocol_version >= 4 {
                    event_payload["message_key"] = serde_json::json!(payload.message_key);
                }
                if let Some(peer_id) = payload.peer_id
                    && protocol_version >= 4
                {
                    event_payload["peer_id"] = serde_json::json!(peer_id);
                }
                serde_json::to_string(&serde_json::json!({
                    "protocol_version": protocol_version,
                    "type": "event",
                    "request_id": request_id,
                    "event": event.as_str(),
                    "payload": event_payload,
                }))
            }
            Self::TelegramResult {
                request_id,
                call_id,
                result,
            } => {
                if protocol_version != 5 {
                    return Err(ExternalError::ProtocolEncode);
                }
                let mut value = serde_json::json!({
                    "protocol_version": 5,
                    "type": "telegram.result",
                    "request_id": request_id,
                    "call_id": call_id,
                });
                match result {
                    Ok(result) => {
                        value["ok"] = serde_json::Value::Bool(true);
                        value["result"] = result.clone();
                    }
                    Err(error) => {
                        value["ok"] = serde_json::Value::Bool(false);
                        let mut error_value = serde_json::Map::new();
                        error_value.insert(
                            "kind".to_owned(),
                            serde_json::Value::String(error.kind.to_owned()),
                        );
                        error_value.insert(
                            "message".to_owned(),
                            serde_json::Value::String(error.message.clone()),
                        );
                        if let Some(code) = error.code {
                            error_value.insert("code".to_owned(), serde_json::json!(code));
                        }
                        if let Some(name) = &error.name {
                            error_value.insert(
                                "name".to_owned(),
                                serde_json::Value::String(name.clone()),
                            );
                        }
                        if let Some(seconds) = error.retry_after_seconds {
                            error_value.insert(
                                "retry_after_seconds".to_owned(),
                                serde_json::json!(seconds),
                            );
                        }
                        value["error"] = serde_json::Value::Object(error_value);
                    }
                }
                serde_json::to_string(&value)
            }
        }
        .map_err(|_| ExternalError::ProtocolEncode)
    }

    pub fn request_id(&self) -> &str {
        match self {
            Self::Initialize { request_id, .. }
            | Self::Execute { request_id, .. }
            | Self::Health { request_id }
            | Self::Shutdown { request_id }
            | Self::Event { request_id, .. } => request_id,
            Self::TelegramResult { request_id, .. } => request_id,
        }
    }
}

fn validate_request_id(value: &serde_json::Value) -> Result<String, ExternalError> {
    let id = get_string(value, "request_id")?;
    if id.is_empty() || id.len() > 64 {
        return Err(ExternalError::ProtocolDecode);
    }
    if !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ExternalError::ProtocolDecode);
    }
    Ok(id)
}

pub fn parse_module_line(line: &str) -> Result<Option<ModuleMessage>, ExternalError> {
    parse_module_line_for(line, PROTOCOL_VERSION)
}

pub fn parse_module_line_for(
    line: &str,
    expected_protocol_version: u32,
) -> Result<Option<ModuleMessage>, ExternalError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(ExternalError::LineTooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|_| ExternalError::ProtocolDecode)?;

    let proto = value
        .get("protocol_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if proto != expected_protocol_version as u64 {
        return Err(ExternalError::ProtocolVersionMismatch);
    }

    let msg_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or(ExternalError::ProtocolDecode)?;

    match msg_type {
        "initialized" => {
            let request_id = validate_request_id(&value)?;
            let module_id = get_string(&value, "module_id")?;
            Ok(Some(ModuleMessage::Initialized {
                request_id,
                module_id,
            }))
        }
        "result" => {
            let request_id = validate_request_id(&value)?;
            let text = get_string(&value, "text")?;
            if text.len() > MAX_RESULT_BYTES {
                return Err(ExternalError::ResultTooLarge);
            }
            Ok(Some(ModuleMessage::Result { request_id, text }))
        }
        "error" => {
            let request_id = validate_request_id(&value)?;
            let code = get_string(&value, "code")?;
            let message = get_string(&value, "message")?;
            if code.chars().count() > MAX_ERROR_MESSAGE_CHARS
                || message.chars().count() > MAX_ERROR_MESSAGE_CHARS
            {
                return Err(ExternalError::ResultTooLarge);
            }
            Ok(Some(ModuleMessage::Error {
                request_id,
                code,
                message,
            }))
        }
        "health" => {
            let request_id = validate_request_id(&value)?;
            Ok(Some(ModuleMessage::Health { request_id }))
        }
        "log" => {
            let request_id = validate_request_id(&value)?;
            let level = get_string(&value, "level")?;
            let message = get_string(&value, "message")?;
            if level.chars().count() > MAX_LOG_MESSAGE_CHARS
                || message.chars().count() > MAX_LOG_MESSAGE_CHARS
            {
                return Err(ExternalError::ResultTooLarge);
            }
            Ok(Some(ModuleMessage::Log {
                request_id,
                level,
                message,
            }))
        }
        "event_result" => {
            if expected_protocol_version < 3 {
                return Err(ExternalError::ProtocolDecode);
            }
            let request_id = validate_request_id(&value)?;
            let actions: &[serde_json::Value] = match value.get("actions") {
                Some(actions) => actions.as_array().ok_or(ExternalError::ProtocolDecode)?,
                None if expected_protocol_version >= 4 => &[],
                None => return Err(ExternalError::ProtocolDecode),
            };
            if actions.len() > MAX_EVENT_ACTIONS {
                return Err(ExternalError::ProtocolDecode);
            }
            let actions = actions
                .iter()
                .map(|action| parse_event_action(action, expected_protocol_version))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(ModuleMessage::EventResult {
                request_id,
                actions,
            }))
        }
        "telegram.invoke" => {
            if expected_protocol_version != 5 {
                return Err(ExternalError::ProtocolDecode);
            }
            let request_id = validate_request_id(&value)?;
            let call_id = get_string(&value, "call_id")?;
            if call_id.is_empty()
                || call_id.len() > 64
                || !call_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(ExternalError::ProtocolDecode);
            }
            let method = get_string(&value, "method")?;
            let params = value
                .get("params")
                .cloned()
                .ok_or(ExternalError::ProtocolDecode)?;
            Ok(Some(ModuleMessage::TelegramInvoke {
                request_id,
                call_id,
                method,
                params,
            }))
        }
        _ => Err(ExternalError::ProtocolDecode),
    }
}

fn parse_event_action(
    value: &serde_json::Value,
    protocol_version: u32,
) -> Result<EventAction, ExternalError> {
    if value.get("type").and_then(|value| value.as_str()) != Some("message.react") {
        return Err(ExternalError::ProtocolDecode);
    }
    let message_ref = get_string(value, "message_ref")?;
    let reactions = if protocol_version == 3 {
        vec![parse_reaction(
            value.get("reaction").ok_or(ExternalError::ProtocolDecode)?,
        )?]
    } else if protocol_version >= 4 {
        let reactions = value
            .get("reactions")
            .and_then(|value| value.as_array())
            .ok_or(ExternalError::ProtocolDecode)?;
        if reactions.len() > MAX_REACTIONS_PER_ACTION {
            return Err(ExternalError::ProtocolDecode);
        }
        reactions
            .iter()
            .map(parse_reaction)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        return Err(ExternalError::ProtocolDecode);
    };
    Ok(EventAction {
        message_ref,
        reactions,
    })
}

fn parse_reaction(value: &serde_json::Value) -> Result<ReactionSpec, ExternalError> {
    match value.get("type").and_then(|value| value.as_str()) {
        Some("emoji") => Ok(ReactionSpec::Emoji(get_string(value, "emoji")?)),
        Some("custom_emoji") => Ok(ReactionSpec::CustomEmoji {
            document_id: get_string(value, "document_id")?,
        }),
        _ => Err(ExternalError::ProtocolDecode),
    }
}

fn get_string(value: &serde_json::Value, key: &str) -> Result<String, ExternalError> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or(ExternalError::ProtocolDecode)
}

pub fn request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed).to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn outbound_lifecycle_allows_strings_above_untrusted_input_guard() {
        let frame = V6OutboundCoreFrame::Event {
            request_id: "12".to_owned(),
            event: MessageEventKind::Created,
            payload: MessageEvent {
                event_id: "e".to_owned(),
                message_ref: "r".to_owned(),
                message_key: "k".to_owned(),
                peer_id: None,
                text: "x".repeat(V6_MAX_JSON_STRING_BYTES + 1),
                outgoing: false,
                entities: vec![],
            },
        };
        let line = frame
            .serialize()
            .expect("trusted outbound event should serialize");
        assert!(line.len() < MAX_LINE_BYTES);
    }

    #[test]
    fn outbound_lifecycle_still_obeys_line_limit() {
        let frame = V6OutboundCoreFrame::Event {
            request_id: "12".to_owned(),
            event: MessageEventKind::Created,
            payload: MessageEvent {
                event_id: "e".to_owned(),
                message_ref: "r".to_owned(),
                message_key: "k".to_owned(),
                peer_id: None,
                text: "x".repeat(MAX_LINE_BYTES),
                outgoing: false,
                entities: vec![],
            },
        };
        assert!(matches!(
            frame.serialize(),
            Err(ExternalError::ProtocolEncode)
        ));
    }

    use super::*;

    #[test]
    fn v6_frames_are_parentless_and_strict() {
        let invoke = r#"{"protocol_version":6,"type":"telegram.invoke","call_id":"call_1","method":"account.updateStatus","params":{"offline":true}}"#;
        let V6ModuleFrame::TelegramInvoke {
            call_id,
            method,
            params,
        } = parse_v6_module_frame(invoke).unwrap();
        assert_eq!(call_id, "call_1");
        assert_eq!(method, "account.updateStatus");
        assert_eq!(params.get(), r#"{"offline":true}"#);
        assert!(parse_v6_module_frame(
            r#"{"protocol_version":6,"type":"telegram.invoke","call_id":"call_1","request_id":"1","method":"account.updateStatus","params":{}}"#
        )
        .is_err());
        assert!(parse_v6_module_frame(
            r#"{"protocol_version":6,"type":"telegram.invoke","call_id":"!","method":"account.updateStatus","params":{}}"#
        )
        .is_err());

        let result =
            serialize_v6_core_result("call_1", Ok(serde_json::json!({"offline": true}))).unwrap();
        assert!(matches!(
            parse_v6_core_frame(&result),
            Ok(V6CoreFrame::TelegramResult { result: Ok(_), .. })
        ));
    }

    #[test]
    fn v6_json_limits_do_not_change_v5_parsing() {
        let deep = (0..=V6_MAX_JSON_DEPTH)
            .fold("true".to_owned(), |value, _| format!("{{\"x\":{value}}}"));
        let line = format!(
            "{{\"protocol_version\":6,\"type\":\"telegram.invoke\",\"call_id\":\"call\",\"method\":\"account.updateStatus\",\"params\":{deep}}}"
        );
        assert!(parse_v6_module_frame(&line).is_err());
        assert!(parse_v6_module_frame(&format!(
            "{{\"protocol_version\":6,\"type\":\"telegram.invoke\",\"call_id\":\"call\",\"method\":\"account.updateStatus\",\"params\":\"{}\"}}",
            "x".repeat(V6_MAX_JSON_STRING_BYTES + 1)
        ))
        .is_err());
        assert!(matches!(
            parse_module_line_for(
                r#"{"protocol_version":5,"type":"telegram.invoke","request_id":"10","call_id":"call","method":"account.updateStatus","params":{"offline":true}}"#,
                5
            ),
            Ok(Some(ModuleMessage::TelegramInvoke { .. }))
        ));
    }

    #[test]
    fn v6_wire_rejects_duplicate_keys_and_parses_all_inbound_kinds() {
        assert!(parse_v6_module_frame(
            r#"{"protocol_version":6,"protocol_version":6,"type":"telegram.invoke","call_id":"call","method":"account.updateStatus","params":{}}"#
        )
        .is_err());
        assert!(parse_v6_core_frame(
            r#"{"protocol_version":6,"type":"telegram.result","call_id":"call","ok":true,"ok":true,"result":true}"#
        )
        .is_err());
        assert!(matches!(
            parse_v6_inbound_frame(
                r#"{"protocol_version":6,"type":"initialized","request_id":"1","module_id":"mod"}"#
            ),
            Ok(V6InboundFrame::Initialized { .. })
        ));
        assert!(matches!(
            parse_v6_inbound_frame(
                r#"{"protocol_version":6,"type":"event_result","request_id":"2","actions":[]}"#
            ),
            Ok(V6InboundFrame::EventResult { .. })
        ));
        assert!(parse_v6_inbound_frame(r#"{"protocol_version":6,"type":"health"}"#).is_err());
    }

    #[test]
    fn v6_event_result_actions_are_validated_at_the_protocol_boundary() {
        // A well-formed action parses into the typed shape.
        assert!(matches!(
            parse_v6_inbound_frame(
                r#"{"protocol_version":6,"type":"event_result","request_id":"2","actions":[{"type":"message.react","message_ref":"r1","reactions":[{"type":"emoji","emoji":"👍"}]}]}"#
            ),
            Ok(V6InboundFrame::EventResult { actions, .. }) if actions.len() == 1
        ));
        // Too many actions must be rejected at the v6 boundary.
        let too_many = format!(
            r#"{{"protocol_version":6,"type":"event_result","request_id":"2","actions":[{}]}}"#,
            [r#"{"type":"message.react","message_ref":"r1","reactions":[]}"#;
                MAX_EVENT_ACTIONS + 1]
                .join(",")
        );
        assert!(matches!(
            parse_v6_inbound_frame(&too_many),
            Err(ExternalError::ProtocolDecode)
        ));
        // Malformed action objects (unknown type) must be rejected.
        assert!(matches!(
            parse_v6_inbound_frame(
                r#"{"protocol_version":6,"type":"event_result","request_id":"2","actions":[{"type":"text.send","message_ref":"r1"}]}"#
            ),
            Err(ExternalError::ProtocolDecode)
        ));
        // Too many reactions per action must be rejected.
        let too_many_reactions = format!(
            r#"{{"protocol_version":6,"type":"event_result","request_id":"2","actions":[{{"type":"message.react","message_ref":"r1","reactions":[{}]}}]}}"#,
            [r#"{"type":"emoji","emoji":"a"}"#; MAX_REACTIONS_PER_ACTION + 1].join(",")
        );
        assert!(matches!(
            parse_v6_inbound_frame(&too_many_reactions),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn v6_error_and_log_messages_enforce_the_documented_limits() {
        let within = "x".repeat(MAX_ERROR_MESSAGE_CHARS);
        assert!(matches!(
            parse_v6_inbound_frame(&format!(
                r#"{{"protocol_version":6,"type":"error","request_id":"2","code":"c","message":"{within}"}}"#
            )),
            Ok(V6InboundFrame::Error { .. })
        ));
        let over = "x".repeat(MAX_ERROR_MESSAGE_CHARS + 1);
        assert!(matches!(
            parse_v6_inbound_frame(&format!(
                r#"{{"protocol_version":6,"type":"error","request_id":"2","code":"c","message":"{over}"}}"#
            )),
            Err(ExternalError::ProtocolDecode)
        ));
        let over_code = "x".repeat(MAX_ERROR_MESSAGE_CHARS + 1);
        assert!(matches!(
            parse_v6_inbound_frame(&format!(
                r#"{{"protocol_version":6,"type":"error","request_id":"2","code":"{over_code}","message":"c"}}"#
            )),
            Err(ExternalError::ProtocolDecode)
        ));
        let within_log = "x".repeat(MAX_LOG_MESSAGE_CHARS);
        assert!(matches!(
            parse_v6_inbound_frame(&format!(
                r#"{{"protocol_version":6,"type":"log","request_id":"2","level":"info","message":"{within_log}"}}"#
            )),
            Ok(V6InboundFrame::Log { .. })
        ));
        let over_log = "x".repeat(MAX_LOG_MESSAGE_CHARS + 1);
        assert!(matches!(
            parse_v6_inbound_frame(&format!(
                r#"{{"protocol_version":6,"type":"log","request_id":"2","level":"info","message":"{over_log}"}}"#
            )),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn v6_outbound_bounds_are_encode_errors() {
        let call_id = "a".repeat(64);
        assert!(
            serialize_v6_core_result(
                &call_id,
                Ok(serde_json::Value::String(
                    "x".repeat(V6_MAX_JSON_STRING_BYTES)
                ))
            )
            .is_ok()
        );
        assert!(matches!(
            serialize_v6_core_result(
                "call",
                Ok(serde_json::Value::String(
                    "x".repeat(V6_MAX_JSON_STRING_BYTES + 1)
                ))
            ),
            Err(ExternalError::ProtocolEncode)
        ));
        assert!(matches!(
            serialize_v6_core_result(
                "call",
                Err(V6CallError {
                    kind: "validation".to_owned(),
                    message: "x".repeat(V6_MAX_JSON_STRING_BYTES + 1),
                })
            ),
            Err(ExternalError::ProtocolEncode)
        ));
        assert!(matches!(
            serialize_v6_core_result(
                "call",
                Ok(serde_json::Value::Array(vec![
                    serde_json::Value::String(
                        "x".repeat(V6_MAX_JSON_STRING_BYTES)
                    );
                    V6_MAX_JSON_COLLECTION_ITEMS
                ]))
            ),
            Err(ExternalError::ProtocolEncode)
        ));
        assert!(matches!(
            parse_v6_module_frame(&"x".repeat(MAX_LINE_BYTES + 1)),
            Err(ExternalError::LineTooLarge)
        ));
    }

    #[test]
    fn v6_core_serializes_lifecycle_frames_with_required_numeric_ids() {
        let initialize = V6OutboundCoreFrame::Initialize {
            request_id: "1".to_owned(),
            module_id: "module".to_owned(),
        };
        let execute = V6OutboundCoreFrame::Execute {
            request_id: "2".to_owned(),
            command: "run".to_owned(),
            arguments: "args".to_owned(),
            argument_entities: vec![],
        };
        let event = V6OutboundCoreFrame::Event {
            request_id: "3".to_owned(),
            event: MessageEventKind::Created,
            payload: MessageEvent {
                event_id: "event".to_owned(),
                message_ref: "message".to_owned(),
                message_key: "key".to_owned(),
                peer_id: Some(1),
                text: "text".to_owned(),
                outgoing: true,
                entities: vec![],
            },
        };
        let health = V6OutboundCoreFrame::Health {
            request_id: "4".to_owned(),
        };
        let shutdown = V6OutboundCoreFrame::Shutdown {
            request_id: "5".to_owned(),
        };
        for frame in [initialize, execute, event, health, shutdown] {
            let value: serde_json::Value =
                serde_json::from_str(&frame.serialize().unwrap()).unwrap();
            assert_eq!(value["protocol_version"], 6);
            assert!(value.get("request_id").is_some());
        }
        assert!(
            V6OutboundCoreFrame::Health {
                request_id: "invalid".to_owned()
            }
            .serialize()
            .is_err()
        );
    }

    #[test]
    fn v6_core_result_has_only_call_correlation() {
        let line = V6OutboundCoreFrame::TelegramResult {
            call_id: "call".to_owned(),
            result: Err(V6CallError {
                kind: "validation".to_owned(),
                message: "bad params".to_owned(),
            }),
        }
        .serialize()
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "telegram.result");
        assert!(value.get("request_id").is_none());
        assert_eq!(value["error"]["kind"], "validation");
        assert!(
            V6OutboundCoreFrame::TelegramResult {
                call_id: "".to_owned(),
                result: Ok(serde_json::Value::Null),
            }
            .serialize()
            .is_err()
        );
    }

    fn sample_event(kind: MessageEventKind) -> CoreMessage {
        CoreMessage::Event {
            request_id: "9".to_owned(),
            event: kind,
            payload: MessageEvent {
                event_id: "evt".to_owned(),
                message_ref: "opaque".to_owned(),
                message_key: "stable".to_owned(),
                peer_id: None,
                text: "Привет 🦀".to_owned(),
                outgoing: false,
                entities: vec![CustomEmojiEntity {
                    offset_utf16: 7,
                    length_utf16: 2,
                    document_id: "5456140674028019486".to_owned(),
                }],
            },
        }
    }

    #[test]
    fn serialize_initialize() {
        let msg = CoreMessage::Initialize {
            request_id: "1".to_owned(),
            module_id: "echo".to_owned(),
        };
        let json = msg.serialize().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["protocol_version"], 2);
        assert_eq!(parsed["type"], "initialize");
        assert_eq!(parsed["request_id"], "1");
        assert_eq!(parsed["module_id"], "echo");
    }

    #[test]
    fn serialize_execute() {
        let msg = CoreMessage::Execute {
            request_id: "2".to_owned(),
            command: "repeat".to_owned(),
            arguments: "Привет".to_owned(),
            argument_entities: Vec::new(),
        };
        let json = msg.serialize().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["protocol_version"], 2);
        assert_eq!(parsed["type"], "execute");
        assert_eq!(parsed["request_id"], "2");
        assert_eq!(parsed["command"], "repeat");
        assert_eq!(parsed["arguments"], "Привет");
        assert!(parsed.get("context").is_none());
    }

    #[test]
    fn v3_and_v4_execute_project_custom_emoji_context() {
        let msg = CoreMessage::Execute {
            request_id: "2".to_owned(),
            command: "manage".to_owned(),
            arguments: "добавить 🦀".to_owned(),
            argument_entities: vec![CustomEmojiEntity {
                offset_utf16: 9,
                length_utf16: 2,
                document_id: "5456140674028019486".to_owned(),
            }],
        };
        for version in [3, 4] {
            let parsed: serde_json::Value =
                serde_json::from_str(&msg.serialize_for(version).unwrap()).unwrap();
            assert_eq!(parsed["context"]["argument_entities"][0]["offset_utf16"], 9);
            assert_eq!(parsed["context"]["argument_entities"][0]["length_utf16"], 2);
            assert_eq!(
                parsed["context"]["argument_entities"][0]["document_id"],
                "5456140674028019486"
            );
        }
    }

    #[test]
    fn serialize_health() {
        let msg = CoreMessage::Health {
            request_id: "3".to_owned(),
        };
        let json = msg.serialize().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "health");
    }

    #[test]
    fn serialize_shutdown() {
        let msg = CoreMessage::Shutdown {
            request_id: "4".to_owned(),
        };
        let json = msg.serialize().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "shutdown");
    }

    #[test]
    fn v3_event_result_preserves_custom_emoji_document_id_as_string() {
        let serialized = sample_event(MessageEventKind::Created)
            .serialize_for(3)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["protocol_version"], 3);
        assert!(value["payload"].get("message_key").is_none());
        assert_eq!(
            value["payload"]["entities"][0]["document_id"],
            "5456140674028019486"
        );
        let reply = r#"{"protocol_version":3,"type":"event_result","request_id":"9","actions":[{"type":"message.react","message_ref":"opaque","reaction":{"type":"custom_emoji","document_id":"5456140674028019486"}}]}"#;
        let parsed = parse_module_line_for(reply, 3).unwrap().unwrap();
        let ModuleMessage::EventResult { actions, .. } = parsed else {
            panic!("expected event result");
        };
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].reactions.len(), 1);
    }

    #[test]
    fn v4_serializes_edited_event_with_stable_key() {
        let serialized = sample_event(MessageEventKind::Edited)
            .serialize_for(4)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["event"], "message.edited");
        assert_eq!(value["payload"]["message_key"], "stable");
        assert!(value["payload"].get("peer_id").is_none());
        assert!(
            sample_event(MessageEventKind::Edited)
                .serialize_for(3)
                .is_err()
        );
    }

    #[test]
    fn v4_serializes_peer_id_when_present() {
        let mut event = sample_event(MessageEventKind::Created);
        let CoreMessage::Event { payload, .. } = &mut event else {
            panic!("expected event");
        };
        payload.peer_id = Some(-1002871795336);
        let serialized = event.serialize_for(4).unwrap();
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["payload"]["peer_id"], -1002871795336_i64);
    }

    #[test]
    fn v3_does_not_serialize_peer_id_when_present() {
        let mut event = sample_event(MessageEventKind::Created);
        let CoreMessage::Event { payload, .. } = &mut event else {
            panic!("expected event");
        };
        payload.peer_id = Some(-1002871795336);
        let serialized = event.serialize_for(3).unwrap();
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert!(value["payload"].get("peer_id").is_none());
    }

    #[test]
    fn v4_parses_reaction_sets_and_empty_removal() {
        let reply = r#"{"protocol_version":4,"type":"event_result","request_id":"9","actions":[{"type":"message.react","message_ref":"opaque","reactions":[{"type":"emoji","emoji":"👍"},{"type":"custom_emoji","document_id":"5456140674028019486"}]}]}"#;
        let parsed = parse_module_line_for(reply, 4).unwrap().unwrap();
        let ModuleMessage::EventResult { actions, .. } = parsed else {
            panic!("expected event result");
        };
        assert_eq!(actions[0].reactions.len(), 2);

        let remove = r#"{"protocol_version":4,"type":"event_result","request_id":"9","actions":[{"type":"message.react","message_ref":"opaque","reactions":[]}]}"#;
        let parsed = parse_module_line_for(remove, 4).unwrap().unwrap();
        let ModuleMessage::EventResult { actions, .. } = parsed else {
            panic!("expected event result");
        };
        assert!(actions[0].reactions.is_empty());
    }

    #[test]
    fn v4_event_result_accepts_omitted_or_empty_actions_for_noop() {
        let omitted = r#"{"protocol_version":4,"type":"event_result","request_id":"42"}"#;
        let parsed = parse_module_line_for(omitted, 4).unwrap().unwrap();
        let ModuleMessage::EventResult { actions, .. } = parsed else {
            panic!("expected event result");
        };
        assert!(actions.is_empty());

        let empty =
            r#"{"protocol_version":4,"type":"event_result","request_id":"42","actions":[]}"#;
        let parsed = parse_module_line_for(empty, 4).unwrap().unwrap();
        let ModuleMessage::EventResult { actions, .. } = parsed else {
            panic!("expected event result");
        };
        assert!(actions.is_empty());

        let malformed =
            r#"{"protocol_version":4,"type":"event_result","request_id":"42","actions":{}}"#;
        assert!(matches!(
            parse_module_line_for(malformed, 4),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn v4_rejects_more_than_three_reactions() {
        let reply = r#"{"protocol_version":4,"type":"event_result","request_id":"9","actions":[{"type":"message.react","message_ref":"opaque","reactions":[{"type":"emoji","emoji":"1"},{"type":"emoji","emoji":"2"},{"type":"emoji","emoji":"3"},{"type":"emoji","emoji":"4"}]}]}"#;
        assert!(matches!(
            parse_module_line_for(reply, 4),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn parse_initialized() {
        let line =
            r#"{"protocol_version":2,"type":"initialized","request_id":"1","module_id":"echo"}"#;
        let msg = parse_module_line(line).unwrap().unwrap();
        assert_eq!(
            msg,
            ModuleMessage::Initialized {
                request_id: "1".to_owned(),
                module_id: "echo".to_owned()
            }
        );
    }

    #[test]
    fn parse_result() {
        let line = r#"{"protocol_version":2,"type":"result","request_id":"2","text":"Привет"}"#;
        let msg = parse_module_line(line).unwrap().unwrap();
        assert_eq!(
            msg,
            ModuleMessage::Result {
                request_id: "2".to_owned(),
                text: "Привет".to_owned()
            }
        );
    }

    #[test]
    fn parse_error() {
        let line = r#"{"protocol_version":2,"type":"error","request_id":"2","code":"BAD_INPUT","message":"Invalid input"}"#;
        let msg = parse_module_line(line).unwrap().unwrap();
        assert_eq!(
            msg,
            ModuleMessage::Error {
                request_id: "2".to_owned(),
                code: "BAD_INPUT".to_owned(),
                message: "Invalid input".to_owned()
            }
        );
    }

    #[test]
    fn parse_health() {
        let line = r#"{"protocol_version":2,"type":"health","request_id":"3"}"#;
        let msg = parse_module_line(line).unwrap().unwrap();
        assert_eq!(
            msg,
            ModuleMessage::Health {
                request_id: "3".to_owned()
            }
        );
    }

    #[test]
    fn parse_log() {
        let line = r#"{"protocol_version":2,"type":"log","request_id":"1","level":"info","message":"hello"}"#;
        let msg = parse_module_line(line).unwrap().unwrap();
        assert_eq!(
            msg,
            ModuleMessage::Log {
                request_id: "1".to_owned(),
                level: "info".to_owned(),
                message: "hello".to_owned()
            }
        );
    }

    #[test]
    fn reject_wrong_protocol_version() {
        let line =
            r#"{"protocol_version":1,"type":"initialized","request_id":"1","module_id":"echo"}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolVersionMismatch)
        ));
    }

    #[test]
    fn reject_unknown_type() {
        let line = r#"{"protocol_version":2,"type":"unknown","request_id":"1"}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn reject_malformed_json() {
        assert!(matches!(
            parse_module_line("not json"),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn reject_oversized_line() {
        let long = "x".repeat(MAX_LINE_BYTES + 1);
        assert!(matches!(
            parse_module_line(&long),
            Err(ExternalError::LineTooLarge)
        ));
    }

    #[test]
    fn reject_oversized_result() {
        let long = "x".repeat(MAX_RESULT_BYTES + 1);
        let line = serde_json::to_string(&serde_json::json!({
            "protocol_version": 2,
            "type": "result",
            "request_id": "1",
            "text": long,
        }))
        .unwrap();
        assert!(matches!(
            parse_module_line(&line),
            Err(ExternalError::ResultTooLarge)
        ));
    }

    #[test]
    fn request_id_is_unique() {
        let id1 = request_id();
        let id2 = request_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn v5_telegram_invoke_preserves_correlation() {
        let invoke = r#"{"protocol_version":5,"type":"telegram.invoke","request_id":"10","call_id":"call_1","method":"account.updateStatus","params":{"offline":true}}"#;
        assert!(matches!(
            parse_module_line_for(invoke, 5).unwrap(),
            Some(ModuleMessage::TelegramInvoke { request_id, call_id, .. })
                if request_id == "10" && call_id == "call_1"
        ));
        assert!(parse_module_line_for(invoke, 4).is_err());
        let missing_parent = r#"{"protocol_version":5,"type":"telegram.invoke","call_id":"call_1","method":"account.updateStatus","params":{"offline":true}}"#;
        assert!(parse_module_line_for(missing_parent, 5).is_err());
    }

    #[test]
    fn v5_telegram_result_uses_the_frozen_success_and_error_envelopes() {
        let success = CoreMessage::TelegramResult {
            request_id: "10".to_owned(),
            call_id: "call_1".to_owned(),
            result: Ok(serde_json::Value::Bool(true)),
        };
        let value: serde_json::Value =
            serde_json::from_str(&success.serialize_for(5).unwrap()).unwrap();
        assert_eq!(value["type"], "telegram.result");
        assert_eq!(value["request_id"], "10");
        assert_eq!(value["call_id"], "call_1");
        assert_eq!(value["ok"], true);
        assert_eq!(value["result"], true);

        let failure = CoreMessage::TelegramResult {
            request_id: "10".to_owned(),
            call_id: "call_2".to_owned(),
            result: Err(TelegramCallError {
                kind: "rpc",
                code: Some(420),
                name: Some("FLOOD_WAIT".to_owned()),
                message: "FLOOD_WAIT".to_owned(),
                retry_after_seconds: Some(7),
            }),
        };
        let value: serde_json::Value =
            serde_json::from_str(&failure.serialize_for(5).unwrap()).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["kind"], "rpc");
        assert_eq!(value["error"]["code"], 420);
        assert_eq!(value["error"]["name"], "FLOOD_WAIT");
        assert_eq!(value["error"]["retry_after_seconds"], 7);
    }

    #[test]
    fn reject_missing_field() {
        let line = r#"{"protocol_version":2,"type":"initialized","request_id":"1"}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn reject_missing_request_id() {
        let line = r#"{"protocol_version":2,"type":"result","text":"hello"}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn reject_non_numeric_request_id() {
        let line = r#"{"protocol_version":2,"type":"result","request_id":"abc","text":"hello"}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn reject_empty_request_id() {
        let line = r#"{"protocol_version":2,"type":"health","request_id":""}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolDecode)
        ));
    }
}
