use grammers_session::types::PeerId;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

const MAX_ENTRIES: usize = 128;

/// Returns true only when Telegram definitively rejected the mutation. All
/// transport/session failures remain ambiguous and therefore fail closed.
pub fn edit_definitely_rejected(error: &grammers_client::InvocationError) -> bool {
    matches!(error, grammers_client::InvocationError::Rpc(_))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExpectedSelfEdit {
    peer_id: PeerId,
    message_id: i32,
    text: String,
}

#[derive(Debug, Default)]
struct Ledger {
    entries: VecDeque<ExpectedSelfEdit>,
}

#[derive(Clone, Debug, Default)]
pub struct SharedSelfEditLedger(Arc<Mutex<Ledger>>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerFull;

impl SharedSelfEditLedger {
    pub fn register(
        &self,
        peer_id: PeerId,
        message_id: i32,
        text: String,
    ) -> Result<(), LedgerFull> {
        let mut ledger = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if ledger.entries.len() >= MAX_ENTRIES {
            return Err(LedgerFull);
        }
        ledger.entries.push_back(ExpectedSelfEdit {
            peer_id,
            message_id,
            text,
        });
        Ok(())
    }
    pub fn consume(&self, peer_id: PeerId, message_id: i32, text: &str) -> bool {
        let mut ledger = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(index) = ledger.entries.iter().position(|entry| {
            entry.peer_id == peer_id && entry.message_id == message_id && entry.text == text
        }) else {
            return false;
        };
        ledger.entries.remove(index).is_some()
    }
    pub fn remove(&self, peer_id: PeerId, message_id: i32, text: &str) {
        let _ = self.consume(peer_id, message_id, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clones_share_bounded_consumable_state() {
        let first = SharedSelfEditLedger::default();
        let second = first.clone();
        let first_peer = PeerId::user(1).unwrap();
        let second_peer = PeerId::user(2).unwrap();
        first.register(first_peer, 2, "ok".into()).unwrap();
        assert!(second.consume(first_peer, 2, "ok"));
        assert!(!first.consume(first_peer, 2, "ok"));
        assert!(!first.consume(second_peer, 2, "ok"));
    }

    #[test]
    fn saturation_is_rejected_without_evicting_unresolved_entries() {
        let ledger = SharedSelfEditLedger::default();
        let peer = PeerId::user(9).unwrap();
        for index in 0..MAX_ENTRIES {
            ledger
                .register(peer, index as i32, index.to_string())
                .unwrap();
        }
        assert!(ledger.register(peer, 999, "new".into()).is_err());
        assert!(ledger.consume(peer, 0, "0"));
        assert!(!ledger.consume(peer, 999, "new"));
    }
}
