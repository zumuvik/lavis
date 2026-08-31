use super::*;

pub struct CreatedEventDispatch {
    handle: ExternalManagerHandle,
    requests: Vec<CreatedEventRequest>,
}

struct CreatedEventRequest {
    descriptor: crate::external_modules::manifest::ExternalModuleDescriptor,
    message_ref: String,
    event: MessageEventKind,
    payload: MessageEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedEventDispatchFailure {
    pub module_id: String,
    pub category: &'static str,
}

#[derive(Debug, Default)]
pub struct CreatedEventDispatchResult {
    pub actions: Vec<EventAction>,
    pub failures: Vec<CreatedEventDispatchFailure>,
}

impl CreatedEventDispatch {
    pub async fn execute(self) -> CreatedEventDispatchResult {
        let mut result = CreatedEventDispatchResult::default();
        let dispatches = self.requests.into_iter().map(|request| {
            let handle = self.handle.clone();
            async move {
                let CreatedEventRequest {
                    descriptor,
                    message_ref,
                    event,
                    payload,
                } = request;
                let module_id = descriptor.id.clone();
                let response = handle.dispatch_event(&module_id, event, payload).await;
                (descriptor, message_ref, response)
            }
        });

        for (descriptor, message_ref, response) in join_all(dispatches).await {
            let module_id = descriptor.id.clone();
            match response {
                Ok((request_id, actions)) => {
                    let scope = EventScope {
                        module_id: module_id.clone(),
                        request_id: request_id.clone(),
                        message_ref,
                    };
                    for action in actions {
                        if let Err(category) =
                            validate_reaction_action(&descriptor, &scope, &request_id, &action)
                        {
                            tracing::warn!(event = "external_reaction_rejected", ?category, module_id = %module_id, "External reaction action rejected");
                            continue;
                        }
                        result.actions.push(action);
                    }
                }
                Err(error) => {
                    result.failures.push(CreatedEventDispatchFailure {
                        module_id,
                        category: external_event_error_category(&error),
                    });
                }
            }
        }
        result
    }
}

fn stable_message_key(peer_id: PeerId, message_id: i32, module_id: &str) -> String {
    fn digest(domain: &str, peer_id: PeerId, message_id: i32, module_id: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        domain.hash(&mut hasher);
        peer_id.hash(&mut hasher);
        message_id.hash(&mut hasher);
        module_id.hash(&mut hasher);
        hasher.finish()
    }
    format!(
        "{:016x}{:016x}",
        digest("lavis-message-key-v1/a", peer_id, message_id, module_id),
        digest("lavis-message-key-v1/b", peer_id, message_id, module_id)
    )
}

impl RuntimeState {
    pub fn prepare_message_event_dispatch(
        &self,
        peer_id: PeerId,
        message_id: i32,
        event: MessageEventKind,
        text: &str,
        outgoing: bool,
        entities: Vec<crate::external_modules::protocol::CustomEmojiEntity>,
    ) -> Option<CreatedEventDispatch> {
        if !self.external_projection_permitted {
            return None;
        }
        let handle = self.external_manager.clone()?;
        let mut requests = Vec::new();
        for descriptor in self
            .external_snapshot
            .descriptors
            .iter()
            .filter(|descriptor| module_can_receive_event(descriptor, event))
        {
            let event_id = crate::external_modules::protocol::request_id();
            let Ok(message_ref) = opaque_message_ref() else {
                tracing::warn!(event = "external_event_reference_failed", module_id = %descriptor.id, "Could not create an external event reference");
                continue;
            };
            let payload = MessageEvent {
                event_id,
                message_ref: message_ref.clone(),
                message_key: stable_message_key(peer_id, message_id, &descriptor.id),
                peer_id: descriptor
                    .capabilities
                    .contains(&ExternalCapability::MessagePeerId)
                    .then(|| peer_id.bot_api_dialog_id())
                    .flatten(),
                text: text.to_owned(),
                outgoing,
                entities: entities.clone(),
            };
            requests.push(CreatedEventRequest {
                descriptor: descriptor.clone(),
                message_ref,
                event,
                payload,
            });
        }
        (!requests.is_empty()).then_some(CreatedEventDispatch { handle, requests })
    }
}

pub(crate) fn external_event_error_category(error: &ExternalError) -> &'static str {
    match error {
        ExternalError::Unavailable => "unavailable",
        ExternalError::ExecutionTimeout => "timeout",
        ExternalError::ProtocolDecode
        | ExternalError::LineTooLarge
        | ExternalError::WrongRequestId
        | ExternalError::WrongModuleId => "protocol",
        ExternalError::ResultTooLarge => "result_too_large",
        ExternalError::ModuleError => "module_error",
        _ => "other",
    }
}
