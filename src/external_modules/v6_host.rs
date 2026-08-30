use super::{
    manifest::{ExternalCapability, ExternalModuleDescriptor},
    v6_handles::{V6HandleKind, V6HandleRegistry},
};
use crate::message_provenance::SharedSelfEditLedger;
use crate::message_provenance::edit_definitely_rejected;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::sync::{Arc, Mutex};

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MessageEditParams {
    pub message: String,
    pub text: String,
}

impl MessageEditParams {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.message.is_empty() {
            return Err("message handle is required");
        }
        if self.text.is_empty() {
            return Err("text is required");
        }
        if self.text.contains('\0') {
            return Err("text contains NUL");
        }
        if self.text.encode_utf16().count() > 4096 {
            return Err("text is too long");
        }
        Ok(())
    }
}

/// Narrow host-side validation boundary. Telegram mutation is deliberately
/// supplied by the runtime integration, while this type owns capability and
/// opaque-handle checks shared by production and tests.
pub struct V6HostExecutor {
    handles: Arc<Mutex<V6HandleRegistry>>,
    can_edit: bool,
    ledger: SharedSelfEditLedger,
}

impl Default for V6HostExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl V6HostExecutor {
    pub fn new() -> Self {
        Self::from_capability(false)
    }
    pub fn from_descriptor(descriptor: &ExternalModuleDescriptor) -> Self {
        Self::from_capability(
            descriptor
                .capabilities
                .contains(&ExternalCapability::MessageEdit),
        )
    }
    fn from_capability(can_edit: bool) -> Self {
        Self::with_registry(
            can_edit,
            Arc::new(Mutex::new(V6HandleRegistry::new())),
            SharedSelfEditLedger::default(),
        )
    }
    pub(crate) fn with_registry(
        can_edit: bool,
        handles: Arc<Mutex<V6HandleRegistry>>,
        ledger: SharedSelfEditLedger,
    ) -> Self {
        Self {
            handles,
            can_edit,
            ledger,
        }
    }
    pub fn validate_edit(&mut self, params: &MessageEditParams) -> Result<(), &'static str> {
        if !self.can_edit {
            return Err("capability denied");
        }
        params.validate()?;
        self.handles
            .lock()
            .map_err(|_| "invalid message handle")?
            .resolve(&params.message, V6HandleKind::Message)
            .map_err(|_| "invalid message handle")
    }
    pub(crate) async fn execute(
        &self,
        params: Box<RawValue>,
    ) -> Result<serde_json::Value, &'static str> {
        if !self.can_edit {
            return Err("capability denied");
        }
        let params: MessageEditParams =
            serde_json::from_str(params.get()).map_err(|_| "invalid params")?;
        params.validate()?;
        let message = self
            .handles
            .lock()
            .map_err(|_| "invalid message handle")?
            .resolve_message(&params.message)
            .map_err(|_| "invalid message handle")?;
        if !message.outgoing() {
            return Err("message is not outgoing");
        }
        self.ledger
            .register(message.peer_id(), message.id(), params.text.clone())
            .map_err(|_| "suppression capacity exhausted")?;
        let input = grammers_client::message::InputMessage::new().text(params.text.clone());
        match message.edit(input).await {
            Ok(_) => Ok(serde_json::Value::Null),
            Err(error) if error.is("MESSAGE_NOT_MODIFIED") => {
                self.ledger
                    .remove(message.peer_id(), message.id(), &params.text);
                Ok(serde_json::Value::Null)
            }
            Err(error) if edit_definitely_rejected(&error) => {
                self.ledger
                    .remove(message.peer_id(), message.id(), &params.text);
                if error.is("MESSAGE_NOT_MODIFIED") {
                    Ok(serde_json::Value::Null)
                } else {
                    Err("edit rejected")
                }
            }
            Err(_) => Err("edit unavailable"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_validation_enforces_strict_text_limits() {
        let base = |text: String| MessageEditParams {
            message: "handle".into(),
            text,
        };
        assert!(base("x".repeat(4096)).validate().is_ok());
        assert!(base("x".repeat(4097)).validate().is_err());
        assert!(base("😀".repeat(2048)).validate().is_ok());
        assert!(base("😀".repeat(2049)).validate().is_err());
        assert!(base("x\0y".into()).validate().is_err());
        assert!(
            serde_json::from_str::<MessageEditParams>(r#"{"message":"h","text":"x","extra":1}"#)
                .is_err()
        );
        assert!(
            V6HostExecutor::new()
                .validate_edit(&base("x".into()))
                .is_err()
        );
    }
}
