//! Protocol engine: pending-request table, timeouts and connection lifecycle
//! (NET-05..08; docs/01-architecture.md §11).
//!
//! This module holds the state the engine needs across PDUs: which requests
//! are awaiting responses, when each was last active, and whether the
//! connection is still usable. It is deliberately free of IO so the whole
//! state machine can be tested deterministically.

use crate::dimse::{Command, StatusState};
use crate::error::{NetworkError, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Options for the engine (§5.3, §5.4).
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Largest PDU this side accepts; also bounds what it sends.
    pub max_pdu_length: u32,
    /// Bound on the PDU send queue; senders await when full (§5.3).
    pub pdu_queue_capacity: usize,
    /// Maximum PDVs batched into one P-DATA-TF; 1 for buggy peers.
    pub max_pdvs_per_pdu: usize,
    /// Opt-in per-request watchdog; `None` disables it (§5.4).
    pub request_timeout: Option<Duration>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            max_pdu_length: 16384,
            pdu_queue_capacity: 16,
            max_pdvs_per_pdu: 16,
            request_timeout: None,
        }
    }
}

/// One request awaiting a response (§5.4).
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub message_id: u16,
    pub command: Command,
    /// Last time a response arrived, a PDU was sent, or the request was queued.
    pub last_activity: Instant,
    /// True once the caller gave up; further responses are discarded.
    pub abandoned: bool,
}

impl PendingRequest {
    pub fn new(command: Command, now: Instant) -> Self {
        PendingRequest {
            message_id: command.message_id,
            command,
            last_activity: now,
            abandoned: false,
        }
    }

    /// True when the watchdog should fire (§5.4).
    pub fn is_timed_out(&self, now: Instant, timeout: Duration) -> bool {
        now.duration_since(self.last_activity) >= timeout
    }
}

/// The pending-request table and message-ID sequence (§5.2, D3).
#[derive(Debug, Default)]
pub struct PendingTable {
    entries: HashMap<u16, PendingRequest>,
    next_message_id: u16,
}

impl PendingTable {
    pub fn new() -> Self {
        // message IDs start at 1; 0 is not a valid ID
        PendingTable { entries: HashMap::new(), next_message_id: 1 }
    }

    /// Allocates the next message ID, skipping IDs still in flight (D3).
    pub fn allocate_message_id(&mut self) -> Result<u16> {
        // at most 65535 requests can be outstanding
        for _ in 0..u16::MAX {
            let id = self.next_message_id;
            self.next_message_id = if self.next_message_id == u16::MAX { 1 } else { self.next_message_id + 1 };
            if !self.entries.contains_key(&id) {
                return Ok(id);
            }
        }
        Err(NetworkError::protocol("all message IDs are in use"))
    }

    /// Registers a request under its message ID.
    pub fn insert(&mut self, request: PendingRequest) {
        self.entries.insert(request.message_id, request);
    }

    pub fn get(&self, message_id: u16) -> Option<&PendingRequest> {
        self.entries.get(&message_id)
    }

    pub fn get_mut(&mut self, message_id: u16) -> Option<&mut PendingRequest> {
        self.entries.get_mut(&message_id)
    }

    pub fn remove(&mut self, message_id: u16) -> Option<PendingRequest> {
        self.entries.remove(&message_id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Marks a request's activity time; called when a response arrives.
    pub fn touch(&mut self, message_id: u16, now: Instant) {
        if let Some(entry) = self.entries.get_mut(&message_id) {
            entry.last_activity = now;
        }
    }

    /// Requests whose watchdog has expired (§5.4).
    pub fn timed_out(&self, now: Instant, timeout: Duration) -> Vec<u16> {
        let mut ids: Vec<u16> = self
            .entries
            .values()
            .filter(|e| !e.abandoned && e.is_timed_out(now, timeout))
            .map(|e| e.message_id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Every outstanding message ID, for failing them on close (§5.5).
    pub fn message_ids(&self) -> Vec<u16> {
        let mut ids: Vec<u16> = self.entries.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// True when the peer's response completes the request (§5.2).
    ///
    /// `Pending` keeps the entry; anything else removes it.
    pub fn apply_response(&mut self, message_id: u16, status: StatusState, now: Instant) -> Option<PendingRequest> {
        let entry = self.entries.get_mut(&message_id)?;
        entry.last_activity = now;
        if status.is_pending() {
            return None;
        }
        self.entries.remove(&message_id)
    }
}

/// P-DATA circuit-breaker state (§5.3).
///
/// When a request times out while its PDVs are still queued, the engine stops
/// accepting new P-DATA for that message and flags the connection, so the
/// client can roll a new association instead of corrupting the stream.
#[derive(Debug, Clone, Default)]
pub struct CircuitBreaker {
    /// Message IDs whose P-DATA may no longer be sent.
    blocked: Vec<u16>,
    /// True once the connection is considered unusable.
    tripped: bool,
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Blocks further P-DATA for `message_id` and trips the connection.
    pub fn trip(&mut self, message_id: u16) {
        if !self.blocked.contains(&message_id) {
            self.blocked.push(message_id);
        }
        self.tripped = true;
    }

    /// True when P-DATA for this message must not be sent.
    pub fn is_blocked(&self, message_id: u16) -> bool {
        self.blocked.contains(&message_id)
    }

    /// True when the connection cannot process further P-DATA.
    pub fn is_tripped(&self) -> bool {
        self.tripped
    }

    /// Clears the breaker for a fresh association.
    pub fn reset(&mut self) {
        self.blocked.clear();
        self.tripped = false;
    }
}

/// Connection lifecycle state (§5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Accepting and sending.
    Open,
    /// A graceful close was requested; new sends are refused.
    Closing,
    Closed,
}

/// Graceful-close bookkeeping (§5.5).
#[derive(Debug, Clone)]
pub struct ConnectionLifecycle {
    state: ConnectionState,
    queued_pdus: usize,
    pending_requests: usize,
}

impl Default for ConnectionLifecycle {
    fn default() -> Self {
        ConnectionLifecycle { state: ConnectionState::Open, queued_pdus: 0, pending_requests: 0 }
    }
}

impl ConnectionLifecycle {
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Mirrors the current queue and pending-request depths.
    pub fn set_load(&mut self, queued_pdus: usize, pending_requests: usize) {
        self.queued_pdus = queued_pdus;
        self.pending_requests = pending_requests;
    }

    /// Requests a graceful close; refuses while work is outstanding (§5.5).
    pub fn try_close(&mut self) -> Result<()> {
        if self.state != ConnectionState::Open {
            return Err(NetworkError::protocol("connection is already closing"));
        }
        if self.queued_pdus > 0 || self.pending_requests > 0 {
            return Err(NetworkError::protocol("cannot close while queues are non-empty"));
        }
        self.state = ConnectionState::Closing;
        Ok(())
    }

    /// Forces the close regardless of outstanding work (§5.5).
    pub fn force_close(&mut self) {
        self.state = ConnectionState::Closed;
        self.queued_pdus = 0;
        self.pending_requests = 0;
    }

    /// Marks the release handshake complete.
    pub fn finish_close(&mut self) {
        self.state = ConnectionState::Closed;
    }

    /// True when sends should be refused.
    pub fn refuses_sends(&self) -> bool {
        self.state != ConnectionState::Open
    }
}

/// Backpressure accounting for the bounded PDU queue (§5.3).
#[derive(Debug, Clone)]
pub struct PduQueue {
    capacity: usize,
    depth: usize,
}

impl PduQueue {
    pub fn new(capacity: usize) -> Self {
        PduQueue { capacity: capacity.max(1), depth: 0 }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Reserves a slot; `false` means the sender must await (§5.3).
    pub fn try_reserve(&mut self) -> bool {
        if self.depth >= self.capacity {
            return false;
        }
        self.depth += 1;
        true
    }

    /// Releases a slot after the PDU is written.
    pub fn release(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    pub fn is_empty(&self) -> bool {
        self.depth == 0
    }

    pub fn clear(&mut self) {
        self.depth = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dimse::{CommandField, StatusState};

    fn request(message_id: u16) -> PendingRequest {
        PendingRequest::new(Command::request(CommandField::CFindRq, message_id), Instant::now())
    }

    #[test]
    fn message_ids_start_at_one_and_skip_in_flight() {
        let mut table = PendingTable::new();
        assert_eq!(table.allocate_message_id().unwrap(), 1);
        let mut in_flight = request(2);
        in_flight.message_id = 2;
        table.insert(in_flight);
        // 2 is taken, so the allocator must return 3
        assert_eq!(table.allocate_message_id().unwrap(), 3, "in-flight IDs are skipped");
    }

    #[test]
    fn message_ids_wrap_around() {
        let mut table = PendingTable { entries: HashMap::new(), next_message_id: u16::MAX };
        assert_eq!(table.allocate_message_id().unwrap(), u16::MAX);
        assert_eq!(table.allocate_message_id().unwrap(), 1, "wraps to 1, never 0");
    }

    #[test]
    fn pending_responses_keep_the_entry() {
        let mut table = PendingTable::new();
        table.insert(request(7));
        let now = Instant::now();
        assert!(
            table.apply_response(7, StatusState::Pending, now).is_none(),
            "a Pending response leaves the request outstanding"
        );
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn final_responses_complete_the_request() {
        let mut table = PendingTable::new();
        table.insert(request(7));
        let done = table.apply_response(7, StatusState::Success, Instant::now());
        assert!(done.is_some(), "Success completes the request");
        assert!(table.is_empty());

        table.insert(request(9));
        assert!(table.apply_response(9, StatusState::Failure, Instant::now()).is_some());
        assert!(table.is_empty(), "Failure also completes");
    }

    #[test]
    fn responses_for_unknown_ids_are_ignored() {
        let mut table = PendingTable::new();
        assert!(table.apply_response(404, StatusState::Success, Instant::now()).is_none());
    }

    #[test]
    fn touch_updates_the_activity_time() {
        let mut table = PendingTable::new();
        let start = Instant::now();
        table.insert(PendingRequest::new(Command::request(CommandField::CFindRq, 1), start));
        let later = start + Duration::from_secs(10);
        table.touch(1, later);
        assert_eq!(table.get(1).unwrap().last_activity, later);
    }

    #[test]
    fn watchdog_reports_expired_requests() {
        let mut table = PendingTable::new();
        let start = Instant::now();
        table.insert(PendingRequest::new(Command::request(CommandField::CFindRq, 1), start));
        table.insert(PendingRequest::new(Command::request(CommandField::CFindRq, 2), start + Duration::from_secs(30)));

        let timeout = Duration::from_secs(20);
        // only the older request has expired
        let expired = table.timed_out(start + Duration::from_secs(25), timeout);
        assert_eq!(expired, vec![1]);
        // later both have
        let expired = table.timed_out(start + Duration::from_secs(60), timeout);
        assert_eq!(expired, vec![1, 2]);
    }

    #[test]
    fn abandoned_requests_do_not_time_out() {
        let mut table = PendingTable::new();
        let start = Instant::now();
        table.insert(PendingRequest::new(Command::request(CommandField::CFindRq, 1), start));
        table.get_mut(1).unwrap().abandoned = true;
        assert!(table.timed_out(start + Duration::from_secs(60), Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn message_ids_lists_everything_outstanding() {
        let mut table = PendingTable::new();
        table.insert(request(5));
        table.insert(request(2));
        assert_eq!(table.message_ids(), vec![2, 5], "sorted for deterministic failure order");
    }

    #[test]
    fn circuit_breaker_blocks_the_message_and_trips_the_connection() {
        let mut breaker = CircuitBreaker::new();
        assert!(!breaker.is_tripped());
        breaker.trip(11);
        assert!(breaker.is_blocked(11));
        assert!(!breaker.is_blocked(12));
        assert!(breaker.is_tripped(), "a trip marks the connection unusable");

        breaker.reset();
        assert!(!breaker.is_tripped());
        assert!(!breaker.is_blocked(11), "a new association clears the blocks");
    }

    #[test]
    fn graceful_close_refuses_while_work_is_outstanding() {
        let mut lifecycle = ConnectionLifecycle::default();
        lifecycle.set_load(1, 0);
        assert!(lifecycle.try_close().is_err(), "queued PDUs block a graceful close");

        lifecycle.set_load(0, 2);
        assert!(lifecycle.try_close().is_err(), "pending requests block a graceful close");

        lifecycle.set_load(0, 0);
        lifecycle.try_close().unwrap();
        assert_eq!(lifecycle.state(), ConnectionState::Closing);
        assert!(lifecycle.refuses_sends());
        assert!(lifecycle.try_close().is_err(), "closing twice is refused");
    }

    #[test]
    fn force_close_ignores_outstanding_work() {
        let mut lifecycle = ConnectionLifecycle::default();
        lifecycle.set_load(5, 3);
        lifecycle.force_close();
        assert_eq!(lifecycle.state(), ConnectionState::Closed);
        assert!(lifecycle.refuses_sends());
    }

    #[test]
    fn pdu_queue_applies_backpressure() {
        let mut queue = PduQueue::new(2);
        assert!(queue.try_reserve());
        assert!(queue.try_reserve());
        assert!(!queue.try_reserve(), "the queue is full");
        assert_eq!(queue.depth(), 2);
        queue.release();
        assert!(queue.try_reserve(), "a released slot is reusable");
    }

    #[test]
    fn pdu_queue_release_never_underflows() {
        let mut queue = PduQueue::new(4);
        queue.release();
        assert_eq!(queue.depth(), 0);
        assert!(queue.is_empty());
    }

    #[test]
    fn engine_options_defaults_match_the_spec() {
        let options = EngineOptions::default();
        assert_eq!(options.max_pdu_length, 16384);
        assert_eq!(options.pdu_queue_capacity, 16);
        assert_eq!(options.max_pdvs_per_pdu, 16);
        assert!(options.request_timeout.is_none(), "the watchdog is opt-in");
    }
}
