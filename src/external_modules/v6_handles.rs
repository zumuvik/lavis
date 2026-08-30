use grammers_client::message::Message;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub const MAX_V6_HANDLES: usize = 256;
pub const V6_HANDLE_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V6HandleKind {
    Peer,
    Message,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum V6HandleTarget {
    Peer(String),
    Message { peer: String, message: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V6HandleError;

#[derive(Debug)]
struct Entry {
    kind: V6HandleKind,
    _target: V6HandleTarget,
    expires: Instant,
    generation: u64,
    authority: Option<Message>,
    editable: bool,
}

/// Process-local opaque handles. The registry deliberately does not expose the
/// underlying Telegram identifiers and is dropped with its owning process.
#[derive(Debug, Default)]
pub struct V6HandleRegistry {
    entries: HashMap<String, Entry>,
    generation: u64,
    peer_handle: Option<String>,
}

#[allow(dead_code)]
impl V6HandleRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn with_generation(generation: u64) -> Self {
        Self {
            entries: HashMap::new(),
            generation,
            peer_handle: None,
        }
    }
    pub(crate) fn issue(&mut self, kind: V6HandleKind) -> Result<String, V6HandleError> {
        self.issue_target(match kind {
            V6HandleKind::Peer => V6HandleTarget::Peer(String::new()),
            V6HandleKind::Message => V6HandleTarget::Message {
                peer: String::new(),
                message: 0,
            },
        })
    }
    pub(crate) fn issue_target(&mut self, target: V6HandleTarget) -> Result<String, V6HandleError> {
        self.purge_expired();
        if matches!(target, V6HandleTarget::Peer(_))
            && let Some(handle) = self.peer_handle.clone()
            && self.entries.contains_key(&handle)
        {
            return Ok(handle);
        }
        if self.entries.len() >= MAX_V6_HANDLES {
            return Err(V6HandleError);
        }
        for _ in 0..4 {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).map_err(|_| V6HandleError)?;
            let handle = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            if self.entries.contains_key(&handle) {
                continue;
            }
            let kind = match target {
                V6HandleTarget::Peer(_) => V6HandleKind::Peer,
                V6HandleTarget::Message { .. } => V6HandleKind::Message,
            };
            self.entries.insert(
                handle.clone(),
                Entry {
                    kind,
                    _target: target.clone(),
                    expires: Instant::now() + V6_HANDLE_TTL,
                    generation: self.generation,
                    authority: None,
                    editable: false,
                },
            );
            if kind == V6HandleKind::Peer {
                self.peer_handle = Some(handle.clone());
            }
            return Ok(handle);
        }
        Err(V6HandleError)
    }
    pub(crate) fn issue_message(&mut self, message: Message) -> Result<String, V6HandleError> {
        self.issue_target_with_authority(
            V6HandleTarget::Message {
                peer: String::new(),
                message: 0,
            },
            Some(message),
            true,
        )
    }
    pub(crate) fn issue_reply_message(
        &mut self,
        message: Message,
    ) -> Result<String, V6HandleError> {
        self.issue_target_with_authority(
            V6HandleTarget::Message {
                peer: String::new(),
                message: 0,
            },
            Some(message),
            false,
        )
    }
    fn issue_target_with_authority(
        &mut self,
        target: V6HandleTarget,
        authority: Option<Message>,
        editable: bool,
    ) -> Result<String, V6HandleError> {
        self.purge_expired();
        if self.entries.len() >= MAX_V6_HANDLES {
            return Err(V6HandleError);
        }
        for _ in 0..4 {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).map_err(|_| V6HandleError)?;
            let handle = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            if self.entries.contains_key(&handle) {
                continue;
            }
            self.entries.insert(
                handle.clone(),
                Entry {
                    kind: V6HandleKind::Message,
                    _target: target.clone(),
                    expires: Instant::now() + V6_HANDLE_TTL,
                    generation: self.generation,
                    authority: authority.clone(),
                    editable,
                },
            );
            return Ok(handle);
        }
        Err(V6HandleError)
    }
    pub(crate) fn resolve(
        &mut self,
        handle: &str,
        kind: V6HandleKind,
    ) -> Result<(), V6HandleError> {
        self.purge_expired();
        match self.entries.get(handle) {
            Some(entry)
                if entry.kind == kind
                    && entry.generation == self.generation
                    && valid_handle(handle) =>
            {
                Ok(())
            }
            _ => Err(V6HandleError),
        }
    }
    pub(crate) fn purge_expired(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, e| e.expires > now);
    }
    pub(crate) fn resolve_message(&mut self, handle: &str) -> Result<Message, V6HandleError> {
        self.purge_expired();
        match self.entries.get(handle) {
            Some(entry)
                if entry.kind == V6HandleKind::Message
                    && entry.generation == self.generation
                    && valid_handle(handle)
                    && entry.editable =>
            {
                entry.authority.clone().ok_or(V6HandleError)
            }
            _ => Err(V6HandleError),
        }
    }
    pub(crate) fn release(&mut self, handle: &str) {
        if self.peer_handle.as_deref() != Some(handle) {
            self.entries.remove(handle);
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn valid_handle(handle: &str) -> bool {
    handle.len() == 64 && handle.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handles_are_opaque_scoped_and_kind_checked() {
        let mut registry = V6HandleRegistry::new();
        let handle = registry.issue(V6HandleKind::Message).unwrap();
        assert_eq!(handle.len(), 64);
        assert!(registry.resolve(&handle, V6HandleKind::Peer).is_err());
        assert!(registry.resolve(&handle, V6HandleKind::Message).is_ok());
        assert!(
            V6HandleRegistry::new()
                .resolve(&handle, V6HandleKind::Message)
                .is_err()
        );
    }

    #[test]
    fn handles_enforce_capacity_form_and_generation() {
        let mut registry = V6HandleRegistry::with_generation(7);
        for _ in 0..MAX_V6_HANDLES {
            registry.issue(V6HandleKind::Message).unwrap();
        }
        assert!(registry.issue(V6HandleKind::Message).is_err());
        assert!(registry.resolve("forged", V6HandleKind::Peer).is_err());
        let fresh = V6HandleRegistry::with_generation(8);
        assert!(fresh.is_empty());
    }

    #[test]
    fn peer_handles_are_reused_and_released_messages_free_capacity() {
        let mut registry = V6HandleRegistry::new();
        let first = registry.issue(V6HandleKind::Peer).unwrap();
        assert_eq!(registry.issue(V6HandleKind::Peer).unwrap(), first);
        let message = registry.issue(V6HandleKind::Message).unwrap();
        registry.release(&message);
        assert!(registry.issue(V6HandleKind::Message).is_ok());
    }
}
