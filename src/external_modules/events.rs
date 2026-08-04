use super::{
    manifest::{
        ExternalAction, ExternalCapability, ExternalModuleDescriptor, ExternalSubscription,
    },
    protocol::{EventAction, MAX_REACTIONS_PER_ACTION, MessageEventKind, ReactionSpec},
};
use std::collections::HashSet;

pub const MAX_EMOJI_CHARS: usize = 32;

pub fn opaque_message_ref() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventScope {
    pub module_id: String,
    pub request_id: String,
    pub message_ref: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionValidationError {
    ActionNotDeclared,
    CapabilityMissing,
    WrongModuleScope,
    WrongRequestScope,
    WrongMessageReference,
    TooManyReactions,
    InvalidReactionCount,
    DuplicateReaction,
    InvalidEmoji,
    InvalidCustomEmojiDocumentId,
}

impl EventScope {
    pub fn validate(
        &self,
        module_id: &str,
        request_id: &str,
        action: &EventAction,
    ) -> Result<(), ReactionValidationError> {
        if self.module_id != module_id {
            return Err(ReactionValidationError::WrongModuleScope);
        }
        if self.request_id != request_id {
            return Err(ReactionValidationError::WrongRequestScope);
        }
        if self.message_ref != action.message_ref {
            return Err(ReactionValidationError::WrongMessageReference);
        }
        Ok(())
    }
}

pub fn module_can_receive_event(
    descriptor: &ExternalModuleDescriptor,
    event: MessageEventKind,
) -> bool {
    let subscription = match event {
        MessageEventKind::Created => ExternalSubscription::MessageCreated,
        MessageEventKind::Edited => ExternalSubscription::MessageEdited,
    };
    descriptor.protocol_version >= 3
        && (event == MessageEventKind::Created || descriptor.protocol_version >= 4)
        && descriptor.subscriptions.contains(&subscription)
        && descriptor
            .capabilities
            .contains(&ExternalCapability::MessageRead)
}

pub fn validate_reaction_action(
    descriptor: &ExternalModuleDescriptor,
    scope: &EventScope,
    request_id: &str,
    action: &EventAction,
) -> Result<(), ReactionValidationError> {
    if !descriptor.actions.contains(&ExternalAction::MessageReact) {
        return Err(ReactionValidationError::ActionNotDeclared);
    }
    if !descriptor
        .capabilities
        .contains(&ExternalCapability::MessageReact)
    {
        return Err(ReactionValidationError::CapabilityMissing);
    }
    scope.validate(&descriptor.id, request_id, action)?;
    if action.reactions.len() > MAX_REACTIONS_PER_ACTION {
        return Err(ReactionValidationError::TooManyReactions);
    }
    if descriptor.protocol_version == 3 && action.reactions.len() != 1 {
        return Err(ReactionValidationError::InvalidReactionCount);
    }
    let mut seen = HashSet::new();
    for reaction in &action.reactions {
        if !seen.insert(reaction) {
            return Err(ReactionValidationError::DuplicateReaction);
        }
        match reaction {
            ReactionSpec::Emoji(_) if !valid_reaction(reaction) => {
                return Err(ReactionValidationError::InvalidEmoji);
            }
            ReactionSpec::CustomEmoji { .. } if !valid_reaction(reaction) => {
                return Err(ReactionValidationError::InvalidCustomEmojiDocumentId);
            }
            _ => {}
        }
    }
    Ok(())
}

fn valid_reaction(reaction: &ReactionSpec) -> bool {
    match reaction {
        ReactionSpec::Emoji(emoji) => {
            !emoji.is_empty()
                && emoji.chars().count() <= MAX_EMOJI_CHARS
                && !emoji.chars().any(|character| {
                    character.is_control()
                        || matches!(
                            character,
                            '\u{061c}'
                                | '\u{200e}'
                                | '\u{200f}'
                                | '\u{202a}'..='\u{202e}'
                                | '\u{2066}'..='\u{2069}'
                        )
                })
        }
        ReactionSpec::CustomEmoji { document_id } => {
            !document_id.is_empty()
                && document_id.bytes().all(|byte| byte.is_ascii_digit())
                && document_id.parse::<i64>().is_ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_modules::manifest::ExternalCommandDescriptor;
    use std::path::PathBuf;

    fn descriptor(protocol_version: u32) -> ExternalModuleDescriptor {
        ExternalModuleDescriptor {
            protocol_version,
            id: "autoreact".to_owned(),
            display_name: "AutoReact".to_owned(),
            version: "1".to_owned(),
            author: "test".to_owned(),
            entrypoint: PathBuf::new(),
            module_dir: PathBuf::new(),
            capabilities: vec![
                ExternalCapability::MessageRead,
                ExternalCapability::MessageReact,
            ],
            default_command: Some("manage".to_owned()),
            subscriptions: if protocol_version >= 4 {
                vec![
                    ExternalSubscription::MessageCreated,
                    ExternalSubscription::MessageEdited,
                ]
            } else {
                vec![ExternalSubscription::MessageCreated]
            },
            telegram_methods: vec![],
            actions: vec![ExternalAction::MessageReact],
            commands: vec![ExternalCommandDescriptor {
                name: "manage".to_owned(),
                summary_ru: "x".to_owned(),
                description_ru: "x".to_owned(),
                usage: "x".to_owned(),
                examples: vec![],
            }],
        }
    }

    fn scope() -> EventScope {
        EventScope {
            module_id: "autoreact".to_owned(),
            request_id: "7".to_owned(),
            message_ref: "opaque".to_owned(),
        }
    }

    #[test]
    fn validates_scoped_v3_reaction() {
        let descriptor = descriptor(3);
        let action = EventAction {
            message_ref: "opaque".to_owned(),
            reactions: vec![ReactionSpec::CustomEmoji {
                document_id: "5456140674028019486".to_owned(),
            }],
        };
        assert!(module_can_receive_event(
            &descriptor,
            MessageEventKind::Created
        ));
        assert!(!module_can_receive_event(
            &descriptor,
            MessageEventKind::Edited
        ));
        assert!(validate_reaction_action(&descriptor, &scope(), "7", &action).is_ok());
        assert_eq!(
            validate_reaction_action(&descriptor, &scope(), "8", &action),
            Err(ReactionValidationError::WrongRequestScope)
        );
    }

    #[test]
    fn v4_accepts_three_reactions_and_empty_removal() {
        let descriptor = descriptor(4);
        assert!(module_can_receive_event(
            &descriptor,
            MessageEventKind::Edited
        ));
        let action = EventAction {
            message_ref: "opaque".to_owned(),
            reactions: vec![
                ReactionSpec::Emoji("👍".to_owned()),
                ReactionSpec::Emoji("❤️".to_owned()),
                ReactionSpec::CustomEmoji {
                    document_id: "5456140674028019486".to_owned(),
                },
            ],
        };
        assert!(validate_reaction_action(&descriptor, &scope(), "7", &action).is_ok());
        let remove = EventAction {
            message_ref: "opaque".to_owned(),
            reactions: Vec::new(),
        };
        assert!(validate_reaction_action(&descriptor, &scope(), "7", &remove).is_ok());
    }

    #[test]
    fn duplicate_reactions_are_rejected() {
        let descriptor = descriptor(4);
        let action = EventAction {
            message_ref: "opaque".to_owned(),
            reactions: vec![
                ReactionSpec::Emoji("👍".to_owned()),
                ReactionSpec::Emoji("👍".to_owned()),
            ],
        };
        assert_eq!(
            validate_reaction_action(&descriptor, &scope(), "7", &action),
            Err(ReactionValidationError::DuplicateReaction)
        );
    }

    #[test]
    fn opaque_references_have_no_scope_components() {
        let first = opaque_message_ref().unwrap();
        let second = opaque_message_ref().unwrap();
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
        assert!(!first.contains("autoreact"));
    }
}
