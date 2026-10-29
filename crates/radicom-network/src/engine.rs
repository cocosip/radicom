//! Protocol engine state: the message IDs with a request in flight
//! (NET-05; docs/01-architecture.md §11).
//!
//! The table is deliberately free of IO so the allocator can be tested
//! deterministically. Deadlines live where they can be armed (the association
//! loop reads PDUs under `request_timeout`), so this module holds no clock and
//! no send queue.

use crate::error::{NetworkError, Result};
use std::collections::HashSet;

/// The message IDs with a request in flight (§11.2, D3).
#[derive(Debug, Default)]
pub struct PendingTable {
    entries: HashSet<u16>,
    next_message_id: u16,
}

impl PendingTable {
    pub fn new() -> Self {
        // message IDs start at 1; 0 is not a valid ID
        PendingTable {
            entries: HashSet::new(),
            next_message_id: 1,
        }
    }

    /// Allocates the next message ID, skipping IDs still in flight (D3).
    pub fn allocate_message_id(&mut self) -> Result<u16> {
        // at most 65535 requests can be outstanding
        for _ in 0..u16::MAX {
            let id = self.next_message_id;
            self.next_message_id = if self.next_message_id == u16::MAX {
                1
            } else {
                self.next_message_id + 1
            };
            if !self.entries.contains(&id) {
                return Ok(id);
            }
        }
        Err(NetworkError::protocol("all message IDs are in use"))
    }

    /// Registers a request's message ID as in flight.
    pub fn insert(&mut self, message_id: u16) {
        self.entries.insert(message_id);
    }

    /// Releases a message ID once its request completed; `false` when it was
    /// not in flight.
    pub fn remove(&mut self, message_id: u16) -> bool {
        self.entries.remove(&message_id)
    }
}

/// Connection lifecycle state for the client's event stream (§11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Accepting and sending.
    Open,
    /// A graceful close was requested; new sends are refused.
    Closing,
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_ids_start_at_one_and_skip_in_flight() {
        let mut table = PendingTable::new();
        assert_eq!(table.allocate_message_id().unwrap(), 1);
        table.insert(2);
        // 2 is taken, so the allocator must return 3
        assert_eq!(
            table.allocate_message_id().unwrap(),
            3,
            "in-flight IDs are skipped"
        );
    }

    #[test]
    fn message_ids_wrap_around() {
        let mut table = PendingTable {
            entries: HashSet::new(),
            next_message_id: u16::MAX,
        };
        assert_eq!(table.allocate_message_id().unwrap(), u16::MAX);
        assert_eq!(
            table.allocate_message_id().unwrap(),
            1,
            "wraps to 1, never 0"
        );
    }

    #[test]
    fn a_finished_request_frees_its_id() {
        let mut table = PendingTable::new();
        table.insert(2);
        assert!(table.remove(2), "the ID was in flight");
        assert!(!table.remove(2), "and only once");
        // the allocator already moved past 2, so the freed ID is not reused
        // by the next call — it simply stops blocking an eventual wrap
        assert_eq!(table.allocate_message_id().unwrap(), 1);
        table.insert(1);
        let mut next = PendingTable::new();
        next.insert(1);
        assert_eq!(
            next.allocate_message_id().unwrap(),
            2,
            "a taken ID is skipped"
        );
    }

    #[test]
    fn message_id_exhaustion_is_reported() {
        let mut table = PendingTable::new();
        // every ID except 0 occupied: the allocator cannot hand one out
        for id in 1..=u16::MAX {
            table.insert(id);
        }
        let error = table
            .allocate_message_id()
            .expect_err("65535 outstanding requests leave no ID");
        assert!(error.to_string().contains("all message IDs are in use"));
    }
}
