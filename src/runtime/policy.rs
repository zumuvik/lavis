use super::*;

impl RuntimeState {
    pub(super) fn authorize_sensitive_command(
        &self,
        policy: SensitiveCommandPolicy,
        context: MessageExecutionContext<'_>,
        saved_messages_peer: Option<PeerId>,
    ) -> Result<(), Response> {
        authorize_sensitive_message(
            policy,
            context.edited,
            context.authored_by_self,
            context.message.peer_id(),
            context.message.id(),
            saved_messages_peer,
        )
        .map_err(|reason| {
            let text = match policy {
                SensitiveCommandPolicy::ModuleMutation => {
                    sensitive_text(self.locale(), SensitiveText::ModuleMutationDenied)
                }
                SensitiveCommandPolicy::Reboot => reason.response(self.locale(), policy),
            };
            Response::plain_with_locale(self.locale(), text)
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SensitiveCommandPolicy {
    ModuleMutation,
    Reboot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SensitiveCommandDenial {
    Edited,
    NotSelfAuthored,
    InvalidMessageId,
    NotSavedMessages,
}

impl SensitiveCommandDenial {
    pub(super) fn response(self, locale: Locale, policy: SensitiveCommandPolicy) -> &'static str {
        match (policy, self) {
            (SensitiveCommandPolicy::ModuleMutation, _) => {
                sensitive_text(locale, SensitiveText::ModuleMutationDenied)
            }
            (SensitiveCommandPolicy::Reboot, _) => {
                sensitive_text(locale, SensitiveText::RebootDenied)
            }
        }
    }
}

pub(super) fn authorize_sensitive_message(
    policy: SensitiveCommandPolicy,
    edited: bool,
    authored_by_self: bool,
    peer_id: PeerId,
    message_id: i32,
    saved_messages_peer: Option<PeerId>,
) -> Result<(), SensitiveCommandDenial> {
    if edited {
        return Err(SensitiveCommandDenial::Edited);
    }
    if !authored_by_self {
        return Err(SensitiveCommandDenial::NotSelfAuthored);
    }
    if message_id <= 0 {
        return Err(SensitiveCommandDenial::InvalidMessageId);
    }
    if policy == SensitiveCommandPolicy::ModuleMutation && saved_messages_peer != Some(peer_id) {
        return Err(SensitiveCommandDenial::NotSavedMessages);
    }
    Ok(())
}
