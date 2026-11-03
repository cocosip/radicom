//! PDV assembly and PDU fragmentation (NET-05/06; docs/01-architecture.md §11).
//!
//! Both directions are pure state machines over byte slices, so the wire
//! behaviour (fragmenting, even padding, command/dataset splitting) is fully
//! unit-testable without a socket.

use crate::error::{NetworkError, Result};
use crate::pdu::{PData, Pdv};
use radicom::io::temp::create_unique_file;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Which stream a PDV belongs to (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Command,
    Data,
}

/// How a spoolable dataset is received (H3).
///
/// fo-dicom writes every C-STORE PDV straight to a `TemporaryFile`
/// (`DicomService.CreateCStoreReceiveStream`) instead of buffering the whole
/// instance. [`PdvAssembler::arm_spill`] selects that path for one dataset
/// stream; this carries its limits.
#[derive(Debug, Clone)]
pub struct SpoolConfig {
    /// Bytes kept in memory before the dataset moves to a file.
    ///
    /// `0` spills from the first PDV, which is fo-dicom's behaviour and the
    /// default; a larger value keeps small instances entirely in memory.
    pub memory_threshold: usize,
    /// Hard bound on one spooled dataset. A larger message is refused.
    ///
    /// This replaces the flat [`PdvAssembler::DEFAULT_MAX_MESSAGE_LENGTH`]
    /// refusal for armed streams, so a valid whole-slide instance above 1 GiB
    /// is no longer rejected.
    pub max_message_length: u64,
    /// Directory for the spool files (`None` = the OS temp directory).
    pub dir: Option<std::path::PathBuf>,
}

impl Default for SpoolConfig {
    fn default() -> Self {
        SpoolConfig {
            memory_threshold: 0,
            max_message_length: Self::DEFAULT_MAX_SPILLED_LENGTH,
            dir: None,
        }
    }
}

impl SpoolConfig {
    /// Largest spooled dataset a default assembler accepts (64 GiB).
    pub const DEFAULT_MAX_SPILLED_LENGTH: u64 = 64 * 1024 * 1024 * 1024;
}

/// A received message body: in memory, or streamed to a file (H3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageBody {
    /// The whole message is in memory (the normal path).
    Memory(bytes::Bytes),
    /// The message was streamed to this file as its PDVs arrived.
    Spilled(Arc<SpoolFile>),
}

impl MessageBody {
    /// The message length in bytes, without reading a spooled file.
    pub fn len(&self) -> u64 {
        match self {
            MessageBody::Memory(bytes) => bytes.len() as u64,
            MessageBody::Spilled(spool) => spool.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes, reading the spool file back when the message was spilled.
    pub fn read_all(&self) -> Result<bytes::Bytes> {
        match self {
            MessageBody::Memory(bytes) => Ok(bytes.clone()),
            MessageBody::Spilled(spool) => spool.read_all(),
        }
    }

    /// The bytes when they never left memory.
    pub fn in_memory(&self) -> Option<&[u8]> {
        match self {
            MessageBody::Memory(bytes) => Some(bytes),
            MessageBody::Spilled(_) => None,
        }
    }

    /// The spool file, when the message was streamed to disk.
    pub fn spool(&self) -> Option<&Arc<SpoolFile>> {
        match self {
            MessageBody::Spilled(spool) => Some(spool),
            MessageBody::Memory(_) => None,
        }
    }
}

impl From<bytes::Bytes> for MessageBody {
    fn from(bytes: bytes::Bytes) -> Self {
        MessageBody::Memory(bytes)
    }
}

/// A dataset streamed to disk, removed when the last reference drops.
///
/// Mirrors fo-dicom's `TemporaryFile`: the file exists only while the received
/// instance does, unless [`SpoolFile::keep`] hands ownership to the caller.
#[derive(Debug)]
pub struct SpoolFile {
    path: std::path::PathBuf,
    len: AtomicU64,
    keep: AtomicBool,
}

impl SpoolFile {
    fn new(path: std::path::PathBuf) -> Self {
        SpoolFile {
            path,
            len: AtomicU64::new(0),
            keep: AtomicBool::new(false),
        }
    }

    /// The file's path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Bytes written so far (the dataset length once it is complete).
    pub fn len(&self) -> u64 {
        self.len.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Keeps the file after the last reference drops; the caller then owns it.
    pub fn keep(&self) {
        self.keep.store(true, Ordering::Relaxed);
    }

    /// Reads the whole spooled message back into memory.
    pub fn read_all(&self) -> Result<bytes::Bytes> {
        let bytes = std::fs::read(&self.path).map_err(NetworkError::Io)?;
        Ok(bytes::Bytes::from(bytes))
    }

    fn add_len(&self, bytes: usize) {
        self.len.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

impl PartialEq for SpoolFile {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for SpoolFile {}

impl Drop for SpoolFile {
    fn drop(&mut self) {
        if !self.keep.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// An in-progress spilled dataset: its final [`SpoolFile`] and the open handle.
#[derive(Debug, Clone)]
struct SpillWriter {
    spool: Arc<SpoolFile>,
    file: Arc<std::fs::File>,
}

impl SpillWriter {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        let mut file = &*self.file;
        file.write_all(bytes).map_err(NetworkError::Io)?;
        self.spool.add_len(bytes.len());
        Ok(())
    }

    fn written(&self) -> u64 {
        self.spool.len()
    }
}

/// Reassembles PDVs into complete message units (§11.2).
///
/// A unit is complete when a PDV with the `last` bit arrives. Buffers are keyed
/// by `(presentation context, stream)`: two requests may interleave their PDVs
/// (an asynchronous operation window above 1 is negotiable, and a C-GET SCP
/// talks on two contexts at once), so one command and one dataset buffer per
/// connection would splice unrelated messages together.
///
/// A message that never completes is capped (NET-05): without a limit a peer
/// can make the receiver buffer forever.
///
/// The cap is enforced twice. Per message, so a single stream cannot grow past
/// `max_message_length`. In total, so a peer that opens many presentation
/// contexts cannot hold one message's worth of bytes per context: without the
/// second check the worst case was 128 contexts × 2 streams × the message limit
/// (NET-05 follow-up).
#[derive(Debug, Clone)]
pub struct PdvAssembler {
    /// Half-assembled message per `(context id, stream)`.
    buffers: std::collections::HashMap<(u8, Stream), bytes::BytesMut>,
    /// Bytes currently held in `buffers`, over all streams. Kept incrementally
    /// so the aggregate check does not walk every context per PDV.
    buffered: usize,
    /// Largest single message this assembler accepts.
    max_message_length: usize,
    /// Largest total buffered across all in-flight messages.
    max_buffered_bytes: usize,
    /// Spool policy for armed dataset streams (H3); `None` never spills.
    spool: Option<SpoolConfig>,
    /// Contexts whose next dataset stream spills (`arm_spill`).
    armed: std::collections::HashSet<u8>,
    /// Dataset streams currently being written to a file, per context.
    spilling: std::collections::HashMap<u8, SpillWriter>,
    /// High-water mark of `buffered`, so a test can prove the spill bounded it.
    peak_buffered: usize,
}

impl Default for PdvAssembler {
    fn default() -> Self {
        PdvAssembler {
            buffers: std::collections::HashMap::new(),
            buffered: 0,
            max_message_length: Self::DEFAULT_MAX_MESSAGE_LENGTH,
            max_buffered_bytes: Self::DEFAULT_MAX_BUFFERED_BYTES,
            spool: None,
            armed: std::collections::HashSet::new(),
            spilling: std::collections::HashMap::new(),
            peak_buffered: 0,
        }
    }
}

/// A completed message unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageUnit {
    pub context_id: u8,
    pub stream: Stream,
    /// The message bytes. Single-PDV messages share the PDV's `Bytes` (no copy);
    /// fragmented ones are frozen from a `BytesMut` exactly once. Empty when
    /// [`Self::spool`] is set.
    pub bytes: bytes::Bytes,
    /// Set instead of `bytes` when the dataset was streamed to a file (H3).
    pub spool: Option<Arc<SpoolFile>>,
}

impl MessageUnit {
    /// The body, in memory or spooled.
    pub fn body(&self) -> MessageBody {
        match &self.spool {
            Some(spool) => MessageBody::Spilled(spool.clone()),
            None => MessageBody::Memory(self.bytes.clone()),
        }
    }
}

impl PdvAssembler {
    /// Largest message a default assembler will buffer (1 GiB).
    ///
    /// Applies to messages that are not armed for spilling. An armed dataset
    /// stream (a C-STORE instance) is bounded by
    /// [`SpoolConfig::max_message_length`] instead, because it never has to be
    /// resident in memory (H3).
    pub const DEFAULT_MAX_MESSAGE_LENGTH: usize = 1 << 30;

    /// Largest total buffered across all in-flight messages (2 GiB).
    ///
    /// Twice the single-message ceiling, so one maximum-size dataset plus an
    /// interleaved command stream still fits, while 128 contexts of large
    /// partial messages are refused instead of filling memory.
    pub const DEFAULT_MAX_BUFFERED_BYTES: usize = Self::DEFAULT_MAX_MESSAGE_LENGTH * 2;

    pub fn new() -> Self {
        Self::default()
    }

    /// An assembler that rejects messages larger than `max_message_length`.
    ///
    /// The aggregate ceiling scales with it (twice the message limit), so a
    /// small limit in a test also bounds the total.
    pub fn with_max_message_length(max_message_length: usize) -> Self {
        PdvAssembler {
            max_message_length,
            max_buffered_bytes: max_message_length.saturating_mul(2),
            ..Self::default()
        }
    }

    /// An assembler that can spill armed dataset streams to a file (H3).
    pub fn with_spool(spool: SpoolConfig) -> Self {
        PdvAssembler {
            spool: Some(spool),
            ..Self::default()
        }
    }

    /// Arms the next dataset stream on `context_id` for spilling (H3).
    ///
    /// The server arms a context as soon as it decodes a C-STORE command, so
    /// the dataset PDVs that follow are written to a temporary file instead of
    /// being buffered. Without a [`SpoolConfig`] this is a no-op.
    pub fn arm_spill(&mut self, context_id: u8) {
        if self.spool.is_some() {
            self.armed.insert(context_id);
        }
    }

    /// True when `context_id` has an armed (or in-progress) dataset spill.
    pub fn is_spilling(&self, context_id: u8) -> bool {
        self.armed.contains(&context_id) || self.spilling.contains_key(&context_id)
    }

    /// The largest value of `buffered` seen so far (test/diagnostic use).
    pub fn peak_buffered(&self) -> usize {
        self.peak_buffered
    }

    /// Feeds one P-DATA-TF, returning every unit it completed.
    pub fn push_pdata(&mut self, pdata: &PData) -> Result<Vec<MessageUnit>> {
        let mut completed = Vec::new();
        for pdv in &pdata.pdvs {
            if let Some(unit) = self.push_pdv(pdv)? {
                completed.push(unit);
            }
        }
        Ok(completed)
    }

    /// Feeds one PDV, returning a unit when this PDV completes one.
    pub fn push_pdv(&mut self, pdv: &Pdv) -> Result<Option<MessageUnit>> {
        let stream = if pdv.is_command {
            Stream::Command
        } else {
            Stream::Data
        };
        // H3: an armed dataset stream goes to a temporary file as it arrives.
        if stream == Stream::Data && self.armed.contains(&pdv.context_id) {
            return self.push_spilled_pdv(pdv);
        }
        let key = (pdv.context_id, stream);
        if pdv.data.len() > self.max_message_length {
            return Err(NetworkError::protocol(
                "DIMSE message exceeds the maximum size",
            ));
        }
        // Fast path: a complete message arriving in one PDV is shared, not copied.
        if pdv.is_last && !self.buffers.contains_key(&key) {
            return Ok(Some(MessageUnit {
                context_id: pdv.context_id,
                stream,
                bytes: pdv.data.clone(),
                spool: None,
            }));
        }
        let buffered = self.buffers.get(&key).map_or(0, bytes::BytesMut::len);
        if buffered + pdv.data.len() > self.max_message_length {
            // drop the partial message: continuing would only buffer more
            self.drop_buffer(key);
            return Err(NetworkError::protocol(
                "DIMSE message exceeds the maximum size",
            ));
        }
        // aggregate ceiling: one message per presentation context is legal, but
        // holding the limit in every context at once is not something a peer
        // needs (NET-05 follow-up)
        if self.buffered + pdv.data.len() > self.max_buffered_bytes {
            self.drop_buffer(key);
            return Err(NetworkError::protocol(
                "buffered DIMSE messages exceed the maximum size",
            ));
        }
        let buffer = self.buffers.entry(key).or_default();
        buffer.extend_from_slice(&pdv.data);
        self.buffered += pdv.data.len();
        self.peak_buffered = self.peak_buffered.max(self.buffered);
        if !pdv.is_last {
            return Ok(None);
        }

        // the message is complete: drop the buffer so idle contexts cost nothing
        let bytes = self.drop_buffer(key).unwrap_or_default().freeze();
        Ok(Some(MessageUnit {
            context_id: pdv.context_id,
            stream,
            bytes,
            spool: None,
        }))
    }

    /// Feeds one dataset PDV of an armed context (H3).
    ///
    /// The message stays in memory up to [`SpoolConfig::memory_threshold`]
    /// bytes, is moved to a temporary file when it outgrows that, and is
    /// bounded by [`SpoolConfig::max_message_length`] rather than by the flat
    /// in-memory ceiling.
    fn push_spilled_pdv(&mut self, pdv: &Pdv) -> Result<Option<MessageUnit>> {
        let config = self
            .spool
            .clone()
            .expect("an armed stream requires a spool config");
        let context_id = pdv.context_id;
        let key = (context_id, Stream::Data);

        if !self.spilling.contains_key(&context_id) {
            let buffered = self.buffers.get(&key).map_or(0, bytes::BytesMut::len);
            let total = buffered + pdv.data.len();
            if total <= config.memory_threshold {
                let buffer = self.buffers.entry(key).or_default();
                buffer.extend_from_slice(&pdv.data);
                self.buffered += pdv.data.len();
                self.peak_buffered = self.peak_buffered.max(self.buffered);
                if !pdv.is_last {
                    return Ok(None);
                }
                // small enough after all: hand it back from memory
                self.armed.remove(&context_id);
                let bytes = self.drop_buffer(key).unwrap_or_default().freeze();
                return Ok(Some(MessageUnit {
                    context_id,
                    stream: Stream::Data,
                    bytes,
                    spool: None,
                }));
            }
            if total as u64 > config.max_message_length {
                self.drop_buffer(key);
                self.armed.remove(&context_id);
                return Err(NetworkError::protocol(
                    "spilled DIMSE message exceeds the maximum size",
                ));
            }
            let prefix = self.drop_buffer(key).unwrap_or_default();
            let writer = self.open_spill(&config, &prefix)?;
            self.spilling.insert(context_id, writer);
        }

        let writer = self.spilling.get_mut(&context_id).expect("spool writer");
        if writer.written() + pdv.data.len() as u64 > config.max_message_length {
            self.spilling.remove(&context_id);
            self.armed.remove(&context_id);
            return Err(NetworkError::protocol(
                "spilled DIMSE message exceeds the maximum size",
            ));
        }
        writer.append(&pdv.data)?;
        if !pdv.is_last {
            return Ok(None);
        }
        let writer = self.spilling.remove(&context_id).expect("spool writer");
        self.armed.remove(&context_id);
        Ok(Some(MessageUnit {
            context_id,
            stream: Stream::Data,
            bytes: bytes::Bytes::new(),
            spool: Some(writer.spool),
        }))
    }

    /// Opens the spool file and moves an in-memory prefix into it.
    fn open_spill(&self, config: &SpoolConfig, prefix: &[u8]) -> Result<SpillWriter> {
        let dir = config.dir.clone().unwrap_or_else(std::env::temp_dir);
        let (path, file) =
            create_unique_file(&dir, "radicom-recv", "dcm").map_err(NetworkError::Io)?;
        let mut writer = SpillWriter {
            spool: Arc::new(SpoolFile::new(path)),
            file: Arc::new(file),
        };
        if !prefix.is_empty() {
            writer.append(prefix)?;
        }
        Ok(writer)
    }

    /// Removes a stream's partial buffer, keeping the aggregate counter honest.
    fn drop_buffer(&mut self, key: (u8, Stream)) -> Option<bytes::BytesMut> {
        let dropped = self.buffers.remove(&key)?;
        self.buffered = self.buffered.saturating_sub(dropped.len());
        Some(dropped)
    }

    /// Bytes buffered for a stream that has not completed yet, over all
    /// presentation contexts.
    pub fn pending_len(&self, stream: Stream) -> usize {
        self.buffers
            .iter()
            .filter(|((_, buffered), _)| *buffered == stream)
            .map(|(_, bytes)| bytes.len())
            .sum()
    }

    /// True when nothing is half-assembled.
    pub fn is_idle(&self) -> bool {
        self.buffers.values().all(|bytes| bytes.is_empty())
            && self.spilling.is_empty()
            && self.armed.is_empty()
    }
}

/// Splits outgoing message bytes into P-DATA-TF PDUs (§11.1).
///
/// The command stream always travels in its own P-DATA-TF, before the dataset
/// stream. Each PDV value is even-padded and no PDU exceeds the negotiated
/// maximum.
#[derive(Debug, Clone)]
pub struct PduSplitter {
    max_pdu: u32,
    max_pdvs_per_pdu: usize,
    /// Optional cap on one command PDV value (fo-dicom `MaxCommandBuffer`).
    max_command_pdv: Option<usize>,
    /// Optional cap on one dataset PDV value (fo-dicom `MaxDataBuffer`).
    max_data_pdv: Option<usize>,
}

impl PduSplitter {
    /// `max_pdu` is the peer's maximum PDU length; `max_pdvs_per_pdu` exists
    /// for buggy peers that only accept one PDV per PDU.
    pub fn new(max_pdu: u32, max_pdvs_per_pdu: usize) -> Self {
        Self::with_limits(max_pdu, max_pdvs_per_pdu, None, None)
    }

    /// A splitter with the interop knobs of fo-dicom `DicomServiceOptions`:
    /// how many PDVs may share a PDU and how large one command / dataset PDV
    /// may be (`MaxCommandBuffer` / `MaxDataBuffer`). `None` uses the whole PDU
    /// for one PDV, which is the previous behaviour (L7).
    pub fn with_limits(
        max_pdu: u32,
        max_pdvs_per_pdu: usize,
        max_command_pdv: Option<usize>,
        max_data_pdv: Option<usize>,
    ) -> Self {
        PduSplitter {
            max_pdu,
            max_pdvs_per_pdu: max_pdvs_per_pdu.max(1),
            max_command_pdv: max_command_pdv.map(|limit| limit.max(1)),
            max_data_pdv: max_data_pdv.map(|limit| limit.max(1)),
        }
    }

    /// The configured cap on one command PDV value, if any.
    pub fn max_command_pdv(&self) -> Option<usize> {
        self.max_command_pdv
    }

    /// The configured cap on one dataset PDV value, if any.
    pub fn max_data_pdv(&self) -> Option<usize> {
        self.max_data_pdv
    }

    /// The peer's maximum PDU length this splitter was built for.
    pub fn max_pdu(&self) -> u32 {
        self.max_pdu
    }

    /// How many PDVs this splitter may batch into one PDU.
    pub fn max_pdvs_per_pdu(&self) -> usize {
        self.max_pdvs_per_pdu
    }

    /// The largest PDV value that fits in one PDU.
    ///
    /// The negotiated maximum limits the PDU body, and each PDV costs six
    /// header bytes. PDV values are even-padded on the
    /// wire, so the usable size is even too: an odd maximum must not become an
    /// odd fragment plus a pad byte (review #17).
    pub fn max_pdv_value(&self) -> usize {
        ((self.max_pdu as usize).saturating_sub(6)) & !1
    }

    /// The largest PDV value this splitter writes for `is_command`.
    ///
    /// A configured `MaxCommandBuffer`/`MaxDataBuffer` may only shrink the
    /// negotiated PDU limit, never exceed it, and the value stays even so the
    /// pad byte never pushes it past the limit.
    fn pdv_value_limit(&self, is_command: bool) -> usize {
        let pdu = self.max_pdv_value();
        let configured = if is_command {
            self.max_command_pdv
        } else {
            self.max_data_pdv
        };
        match configured {
            Some(limit) => (limit.min(pdu)).max(2) & !1,
            None => pdu,
        }
    }

    /// Splits one message stream into PDUs (§11.1).
    ///
    /// Borrowed form: copies `data` into the PDV buffers. Callers that already
    /// own the encoded bytes use [`Self::split_bytes`] to skip that copy.
    pub fn split(&self, context_id: u8, is_command: bool, data: &[u8]) -> Result<Vec<PData>> {
        self.split_bytes(context_id, is_command, bytes::Bytes::copy_from_slice(data))
    }

    /// Splits an owned message into PDUs without copying its bytes: every PDV
    /// is a `Bytes::slice` of `data` (§11.2).
    ///
    /// The negotiated maximum excludes the six-byte fixed PDU header (PS3.8 D.1).
    pub fn split_bytes(
        &self,
        context_id: u8,
        is_command: bool,
        data: bytes::Bytes,
    ) -> Result<Vec<PData>> {
        let body_limit = self.max_pdu as usize;
        // 6 bytes for the PDV header plus a non-empty even-padded value
        if body_limit < 8 {
            return Err(NetworkError::protocol(
                "maximum PDU length leaves no room for a PDV",
            ));
        }
        let mut pdus: Vec<PData> = Vec::new();
        let mut current = PData::default();
        // bytes already used by the PDVs batched into `current`
        let mut used = 0usize;

        if data.is_empty() {
            current.pdvs.push(Pdv {
                context_id,
                is_command,
                is_last: true,
                data: bytes::Bytes::new(),
            });
            pdus.push(current);
            return Ok(pdus);
        }

        // a configured MaxCommandBuffer/MaxDataBuffer caps every PDV value of
        // this stream, even when the PDU has more room (L7)
        let value_limit = self.pdv_value_limit(is_command);
        let mut offset = 0usize;
        while offset < data.len() {
            // A PDV costs 6 header bytes plus its value, which is even-padded on
            // the wire. Keep the slice itself even so the padding it would need
            // is already counted in the limit (review #17).
            let available = body_limit.saturating_sub(used);
            if available <= 6 {
                pdus.push(std::mem::take(&mut current));
                used = 0;
                continue;
            }
            // `available` is either the full body or a leftover of at most one
            // byte after a flush, so this slice is never empty: no zero-length
            // PDV can be emitted.
            let room = ((available - 6) & !1).min(value_limit);
            let end = (offset + room).min(data.len());
            let is_last = end == data.len();
            let len = end - offset;
            current.pdvs.push(Pdv {
                context_id,
                is_command,
                is_last,
                data: data.slice(offset..end),
            });
            // the value is even-padded on the wire, which costs a byte
            used += 6 + len + len % 2;
            offset = end;
            if current.pdvs.len() >= self.max_pdvs_per_pdu || used >= body_limit {
                pdus.push(std::mem::take(&mut current));
                used = 0;
            }
        }
        if !current.pdvs.is_empty() {
            pdus.push(current);
        }
        Ok(pdus)
    }

    /// Splits a command followed by an optional dataset (§11.1: the command
    /// stream is always its own P-DATA-TF).
    pub fn split_message(
        &self,
        context_id: u8,
        command: &[u8],
        dataset: Option<&[u8]>,
    ) -> Result<Vec<PData>> {
        self.split_message_bytes(
            context_id,
            bytes::Bytes::copy_from_slice(command),
            dataset.map(bytes::Bytes::copy_from_slice),
        )
    }

    /// Owned-message variant of [`Self::split_message`] (no per-PDV copy).
    pub fn split_message_bytes(
        &self,
        context_id: u8,
        command: bytes::Bytes,
        dataset: Option<bytes::Bytes>,
    ) -> Result<Vec<PData>> {
        let mut pdus = self.split_bytes(context_id, true, command)?;
        if let Some(dataset) = dataset {
            pdus.extend(self.split_bytes(context_id, false, dataset)?);
        }
        Ok(pdus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdv(context_id: u8, is_command: bool, is_last: bool, data: &[u8]) -> Pdv {
        Pdv {
            context_id,
            is_command,
            is_last,
            data: bytes::Bytes::copy_from_slice(data),
        }
    }

    #[test]
    fn assembles_a_single_command_unit() {
        let mut assembler = PdvAssembler::new();
        let units = assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(1, true, true, &[1, 2, 3])],
            })
            .unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].stream, Stream::Command);
        assert_eq!(units[0].context_id, 1);
        assert_eq!(units[0].bytes, vec![1, 2, 3]);
        assert!(assembler.is_idle());
    }

    #[test]
    fn a_single_pdv_message_is_shared_not_copied() {
        let mut assembler = PdvAssembler::new();
        let source = bytes::Bytes::from_static(b"single");
        let p = Pdv {
            context_id: 1,
            is_command: true,
            is_last: true,
            data: source.clone(),
        };
        let unit = assembler.push_pdv(&p).unwrap().unwrap();
        assert_eq!(
            unit.bytes.as_ptr(),
            source.as_ptr(),
            "a one-PDV message shares its bytes"
        );
        assert!(assembler.is_idle());
    }

    #[test]
    fn assembles_a_command_split_across_pdvs() {
        let mut assembler = PdvAssembler::new();
        assert!(assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(1, true, false, &[1, 2])]
            })
            .unwrap()
            .is_empty());
        assert_eq!(assembler.pending_len(Stream::Command), 2);
        let units = assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(1, true, true, &[3, 4])],
            })
            .unwrap();
        assert_eq!(
            units[0].bytes,
            vec![1, 2, 3, 4],
            "fragments concatenate in order"
        );
    }

    #[test]
    fn assembles_a_command_split_across_pdus() {
        let mut assembler = PdvAssembler::new();
        assert!(assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(1, true, false, &[1])]
            })
            .unwrap()
            .is_empty());
        assert!(assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(1, true, false, &[2])]
            })
            .unwrap()
            .is_empty());
        let units = assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(1, true, true, &[3])],
            })
            .unwrap();
        assert_eq!(units[0].bytes, vec![1, 2, 3]);
    }

    #[test]
    fn command_and_dataset_streams_stay_separate() {
        let mut assembler = PdvAssembler::new();
        let units = assembler
            .push_pdata(&PData {
                pdvs: vec![
                    pdv(1, true, true, &[0xAA]),
                    pdv(1, false, false, &[1, 2]),
                    pdv(1, false, true, &[3]),
                ],
            })
            .unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].stream, Stream::Command);
        assert_eq!(units[0].bytes, vec![0xAA]);
        assert_eq!(units[1].stream, Stream::Data);
        assert_eq!(
            units[1].bytes,
            vec![1, 2, 3],
            "the dataset unit spans both PDVs"
        );
    }

    #[test]
    fn multiple_units_in_one_pdata() {
        let mut assembler = PdvAssembler::new();
        let units = assembler
            .push_pdata(&PData {
                pdvs: vec![
                    pdv(1, true, true, &[1]),
                    pdv(1, false, true, &[2]),
                    pdv(1, true, true, &[3]),
                    pdv(1, false, true, &[4]),
                ],
            })
            .unwrap();
        assert_eq!(units.len(), 4, "two complete messages in one P-DATA-TF");
        assert_eq!(
            units.iter().map(|u| u.bytes[0]).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn interleaved_contexts_do_not_corrupt_each_other() {
        // two messages on two presentation contexts may interleave PDV
        // by PDV; each must reassemble from its own buffer, not from a shared
        // one that splices the two together.
        let mut assembler = PdvAssembler::new();
        assert!(assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(1, true, false, &[0x10, 0x11])]
            })
            .unwrap()
            .is_empty());
        assert!(assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(3, true, false, &[0x30, 0x31])]
            })
            .unwrap()
            .is_empty());
        assert_eq!(
            assembler.pending_len(Stream::Command),
            4,
            "both contexts hold a fragment"
        );

        // context 3 finishes first
        let units = assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(3, true, true, &[0x32])],
            })
            .unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].context_id, 3);
        assert_eq!(
            units[0].bytes,
            vec![0x30, 0x31, 0x32],
            "context 3 keeps its own bytes"
        );

        let units = assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(1, true, true, &[0x12])],
            })
            .unwrap();
        assert_eq!(units[0].context_id, 1);
        assert_eq!(
            units[0].bytes,
            vec![0x10, 0x11, 0x12],
            "context 1 keeps its own bytes"
        );
        assert!(assembler.is_idle());
    }

    #[test]
    fn interleaved_command_and_dataset_streams_across_contexts() {
        // a C-GET SCP sends a command on the retrieve context while a dataset
        // is still arriving on the storage context
        let mut assembler = PdvAssembler::new();
        assert!(assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(1, false, false, &[0x01, 0x02])]
            })
            .unwrap()
            .is_empty());
        let units = assembler
            .push_pdata(&PData {
                pdvs: vec![pdv(3, true, true, &[0xAA]), pdv(1, false, true, &[0x03])],
            })
            .unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(
            (
                units[0].context_id,
                units[0].stream,
                units[0].bytes.to_vec()
            ),
            (3, Stream::Command, vec![0xAA])
        );
        assert_eq!(
            (
                units[1].context_id,
                units[1].stream,
                units[1].bytes.to_vec()
            ),
            (1, Stream::Data, vec![0x01, 0x02, 0x03]),
            "the dataset on context 1 is not touched by the command on context 3"
        );
    }

    #[test]
    fn splitter_fragments_large_messages() {
        // A 24-byte body has six PDV header bytes and 18 payload bytes.
        let splitter = PduSplitter::new(24, 16);
        assert_eq!(splitter.max_pdv_value(), 18);
        let data = vec![0xAB; 40];
        let pdus = splitter.split(1, true, &data).unwrap();
        assert_eq!(pdus.len(), 3, "each PDV fills its PDU");
        assert_eq!(
            pdus.iter()
                .map(|p| p.pdvs[0].data.len())
                .collect::<Vec<_>>(),
            vec![18, 18, 4]
        );
        let total: usize = pdus
            .iter()
            .flat_map(|p| p.pdvs.iter().map(|d| d.data.len()))
            .sum();
        assert_eq!(total, 40, "every byte is carried exactly once");
        // only the very last PDV carries the last bit
        let last_flags: Vec<bool> = pdus
            .iter()
            .flat_map(|p| p.pdvs.iter().map(|d| d.is_last))
            .collect();
        assert_eq!(last_flags, vec![false, false, true]);
    }

    #[test]
    fn a_pdu_never_exceeds_the_negotiated_maximum() {
        // The negotiated maximum limits the body, excluding the six-byte header.
        let splitter = PduSplitter::new(16384, 16);
        let input: Vec<u8> = (0..(1 << 20)).map(|i| (i % 251) as u8).collect();
        let pdus = splitter.split(1, false, &input).unwrap();
        assert!(pdus.len() > 1, "the message is fragmented");
        let mut largest = 0;
        let mut joined = Vec::new();
        for pdu in pdus {
            let wire = crate::pdu::encode(&crate::pdu::Pdu::PData(pdu)).unwrap();
            assert!(
                wire.len() - 6 <= 16384,
                "a {} byte PDU exceeds the negotiated 16384 bytes",
                wire.len()
            );
            largest = largest.max(wire.len());
            let decoded = crate::pdu::decode(&wire).unwrap();
            let crate::pdu::Pdu::PData(back) = decoded else {
                panic!("expected a P-DATA-TF");
            };
            for pdv in back.pdvs {
                joined.extend_from_slice(&pdv.data);
            }
        }
        assert_eq!(joined, input, "no bytes are lost or duplicated");
        assert!(
            largest > 16370,
            "full frames use the whole maximum, got {largest}"
        );
    }

    #[test]
    fn split_bytes_matches_split_without_copying() {
        // §11.2: the owned-bytes path must produce byte-identical PDUs to the
        // borrowed path; it only avoids the per-PDV copy.
        let splitter = PduSplitter::new(24, 4);
        let data: Vec<u8> = (0..80u8).collect();
        let borrowed = splitter.split(7, true, &data).unwrap();
        let owned = splitter
            .split_bytes(7, true, bytes::Bytes::from(data.clone()))
            .unwrap();
        assert_eq!(borrowed, owned);

        let command = &data[..10];
        let dataset = &data[10..];
        let msg_borrowed = splitter.split_message(7, command, Some(dataset)).unwrap();
        let msg_owned = splitter
            .split_message_bytes(
                7,
                bytes::Bytes::copy_from_slice(command),
                Some(bytes::Bytes::copy_from_slice(dataset)),
            )
            .unwrap();
        assert_eq!(msg_borrowed, msg_owned);
    }

    #[test]
    fn splitter_respects_max_pdvs_per_pdu() {
        let splitter = PduSplitter::new(24, 1);
        let data = vec![0xCD; 40];
        let pdus = splitter.split(1, false, &data).unwrap();
        assert_eq!(pdus.len(), 3, "one 18-byte PDV per PDU");
        assert!(
            pdus.iter().all(|p| p.pdvs.len() == 1),
            "one PDV per PDU mode"
        );
    }

    #[test]
    fn splitter_batches_pdvs_when_allowed() {
        // max PDU 1024 ⇒ 1018-byte chunks; 4 PDVs of ~300 bytes fit in batches
        let splitter = PduSplitter::new(1024, 4);
        let data = vec![0xEE; 3000];
        let pdus = splitter.split(1, true, &data).unwrap();
        assert!(pdus.iter().all(|p| p.pdvs.len() <= 4));
        let total: usize = pdus
            .iter()
            .flat_map(|p| p.pdvs.iter().map(|d| d.data.len()))
            .sum();
        assert_eq!(total, 3000);
    }

    #[test]
    fn splitter_keeps_an_empty_message_as_one_pdv() {
        let splitter = PduSplitter::new(16384, 16);
        let pdus = splitter.split(1, true, &[]).unwrap();
        assert_eq!(pdus.len(), 1);
        assert_eq!(pdus[0].pdvs.len(), 1);
        assert!(pdus[0].pdvs[0].is_last);
        assert!(pdus[0].pdvs[0].data.is_empty());
    }

    #[test]
    fn split_message_sends_the_command_first_in_its_own_pdu() {
        let splitter = PduSplitter::new(16384, 16);
        let pdus = splitter
            .split_message(1, &[0xAA; 10], Some(&[0xBB; 10]))
            .unwrap();
        assert_eq!(pdus.len(), 2, "command and dataset are separate P-DATA-TFs");
        assert!(pdus[0].pdvs.iter().all(|d| d.is_command));
        assert!(pdus[1].pdvs.iter().all(|d| !d.is_command));
    }

    #[test]
    fn split_message_without_a_dataset() {
        let splitter = PduSplitter::new(16384, 16);
        let pdus = splitter.split_message(1, &[0xAA; 4], None).unwrap();
        assert_eq!(pdus.len(), 1);
    }

    #[test]
    fn zero_max_pdu_is_rejected() {
        let splitter = PduSplitter::new(0, 16);
        assert!(splitter.split(1, true, &[1, 2]).is_err());
    }

    #[test]
    fn an_odd_maximum_pdu_survives_a_real_encode_decode_round_trip() {
        // review #17: with an odd negotiated maximum the fragments used to be
        // odd, so the encoder added a pad byte per fragment, the first PDU
        // exceeded the limit, and the reassembled data grew by the padding.
        let splitter = PduSplitter::new(16385, 1);
        let input: Vec<u8> = (0..20000).map(|i| (i % 251) as u8).collect();
        let pdus = splitter.split(1, false, &input).unwrap();

        let mut joined = Vec::new();
        for pdu in pdus {
            let wire = crate::pdu::encode(&crate::pdu::Pdu::PData(pdu)).unwrap();
            assert!(
                wire.len() - 6 <= 16385,
                "a {} byte PDU exceeds the negotiated 16385 bytes",
                wire.len()
            );
            let decoded = crate::pdu::decode(&wire).unwrap();
            let crate::pdu::Pdu::PData(back) = decoded else {
                panic!("expected a P-DATA-TF");
            };
            for pdv in back.pdvs {
                joined.extend_from_slice(&pdv.data);
            }
        }
        assert_eq!(
            joined.len(),
            input.len(),
            "no padding leaked into the message"
        );
        assert_eq!(joined, input, "the data round-trips unchanged");
    }

    #[test]
    fn round_trip_through_split_and_assemble() {
        // the splitter's output must reassemble to the original bytes
        let splitter = PduSplitter::new(32, 3);
        let data = vec![0x5A; 100];
        let pdus = splitter.split(7, false, &data).unwrap();
        let mut assembler = PdvAssembler::new();
        let mut units = Vec::new();
        for pdu in pdus {
            units.extend(assembler.push_pdata(&pdu).unwrap());
        }
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].bytes, data, "split then assemble is lossless");
        assert_eq!(units[0].context_id, 7);
        assert_eq!(units[0].stream, Stream::Data);
    }

    #[test]
    fn a_message_at_the_limit_is_accepted() {
        let mut assembler = PdvAssembler::with_max_message_length(4);
        let unit = assembler
            .push_pdv(&pdv(1, true, true, &[1, 2, 3, 4]))
            .unwrap()
            .expect("exactly at the limit");
        assert_eq!(unit.bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_fragmented_message_that_exceeds_the_limit_is_rejected() {
        // NET-05: a peer that keeps sending non-last PDVs must be stopped
        // instead of growing the buffer until the process runs out of memory.
        let mut assembler = PdvAssembler::with_max_message_length(8);
        assert!(assembler
            .push_pdv(&pdv(1, false, false, &[0; 6]))
            .unwrap()
            .is_none());
        let error = assembler
            .push_pdv(&pdv(1, false, false, &[0; 6]))
            .expect_err("the second fragment crosses the limit");
        assert!(
            error.to_string().contains("maximum size"),
            "unexpected error: {error}"
        );
        assert!(
            assembler.is_idle(),
            "the partial message is dropped, not kept for later"
        );
    }

    #[test]
    fn a_single_oversized_pdv_is_rejected() {
        let mut assembler = PdvAssembler::with_max_message_length(4);
        assert!(assembler.push_pdv(&pdv(1, true, true, &[0; 5])).is_err());
    }

    #[test]
    fn buffered_bytes_are_capped_across_presentation_contexts() {
        // NET-05 follow-up: the per-message cap alone let a peer hold one
        // message per (context, stream) — up to 128 x 2 — so total memory was
        // unbounded even though every single message was within the limit.
        let mut assembler = PdvAssembler::with_max_message_length(4);
        // two contexts each hold a 4-byte incomplete message: the aggregate
        // ceiling is 8, so this is exactly full
        assert!(assembler
            .push_pdv(&pdv(1, false, false, &[0; 4]))
            .unwrap()
            .is_none());
        assert!(assembler
            .push_pdv(&pdv(2, false, false, &[0; 4]))
            .unwrap()
            .is_none());

        let error = assembler
            .push_pdv(&pdv(3, false, false, &[0; 1]))
            .expect_err("a third context would exceed the total");
        assert!(
            error.to_string().contains("buffered DIMSE messages"),
            "unexpected error: {error}"
        );

        // completing a message frees its bytes, so the next context fits again
        let unit = assembler
            .push_pdv(&pdv(1, false, true, &[]))
            .expect("the first message completes");
        assert!(unit.is_some());
        assert!(assembler.push_pdv(&pdv(3, false, false, &[0; 1])).is_ok());
    }

    #[test]
    fn round_trip_preserves_the_command_dataset_boundary() {
        let splitter = PduSplitter::new(64, 2);
        let command = vec![0x11; 30];
        let dataset = vec![0x22; 70];
        let pdus = splitter.split_message(3, &command, Some(&dataset)).unwrap();
        let mut assembler = PdvAssembler::new();
        let mut units = Vec::new();
        for pdu in pdus {
            units.extend(assembler.push_pdata(&pdu).unwrap());
        }
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].stream, Stream::Command);
        assert_eq!(units[0].bytes, command);
        assert_eq!(units[1].stream, Stream::Data);
        assert_eq!(units[1].bytes, dataset);
    }

    #[test]
    fn an_odd_body_limit_flushes_without_an_empty_pdv() {
        // 19 > 25-6 = 19 is odd: after the first slice only one byte is left,
        // which must start a new PDU, never a zero-length PDV (review #17).
        let splitter = PduSplitter::new(25, 16);
        let data = vec![0x5A; 60];
        let pdus = splitter.split(1, false, &data).unwrap();
        assert!(pdus
            .iter()
            .all(|pdu| pdu.pdvs.iter().all(|pdv| !pdv.data.is_empty())));
        let total: usize = pdus
            .iter()
            .flat_map(|pdu| pdu.pdvs.iter())
            .map(|pdv| pdv.data.len())
            .sum();
        assert_eq!(total, data.len());
    }

    /// A directory for one spill test, unique per process and tag.
    fn spill_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("radicom-spill-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A dataset PDV of `len` repeated bytes.
    fn dataset_pdv(context_id: u8, last: bool, len: usize) -> Pdv {
        Pdv {
            context_id,
            is_command: false,
            is_last: last,
            data: bytes::Bytes::from(vec![0x5A; len]),
        }
    }

    #[test]
    fn an_armed_dataset_streams_to_a_file_instead_of_the_buffer() {
        // H3: fo-dicom writes every C-STORE PDV to a TemporaryFile
        // (DicomService.CreateCStoreReceiveStream). An armed stream must never
        // hold the instance in memory.
        let dir = spill_dir("armed");
        let mut assembler = PdvAssembler::with_spool(SpoolConfig {
            memory_threshold: 0,
            max_message_length: 64 * 1024 * 1024,
            dir: Some(dir.clone()),
        });
        assembler.arm_spill(1);
        let mut completed = None;
        for index in 0..32 {
            let pdv = dataset_pdv(1, index == 31, 64 * 1024);
            completed = assembler.push_pdv(&pdv).unwrap().or(completed);
        }
        let unit = completed.expect("the last PDV completes the dataset");
        assert!(unit.bytes.is_empty(), "no copy is kept in memory");
        let spool = unit.spool.clone().expect("the dataset was spilled");
        assert_eq!(spool.len(), 2 * 1024 * 1024);
        assert_eq!(
            std::fs::metadata(spool.path()).unwrap().len(),
            2 * 1024 * 1024
        );
        assert!(
            assembler.peak_buffered() < 64 * 1024,
            "peak buffer {} must stay below one instance",
            assembler.peak_buffered()
        );
        assert!(!assembler.is_spilling(1), "the spill ends with the message");
        assert!(assembler.is_idle());
        let path = spool.path().to_path_buf();
        drop(spool);
        drop(unit);
        drop(assembler);
        assert!(!path.exists(), "the temp file dies with the message");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_kept_spool_file_survives_the_message() {
        // StoreSink::SpoolToTemp adopts the received file instead of copying it
        let dir = spill_dir("keep");
        let mut assembler = PdvAssembler::with_spool(SpoolConfig {
            memory_threshold: 0,
            dir: Some(dir.clone()),
            ..SpoolConfig::default()
        });
        assembler.arm_spill(1);
        let unit = assembler
            .push_pdv(&dataset_pdv(1, true, 4096))
            .unwrap()
            .expect("one PDV completes the dataset");
        let spool = unit.spool.clone().expect("the dataset was spilled");
        spool.keep();
        let path = spool.path().to_path_buf();
        drop(unit);
        drop(assembler);
        assert!(path.exists(), "a kept spool file outlives the message");
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_armed_dataset_above_the_in_memory_cap_is_still_accepted() {
        // The flat DEFAULT_MAX_MESSAGE_LENGTH refusal no longer applies to a
        // spilled dataset; 64 KiB exercises the path a multi-GB instance takes.
        let dir = spill_dir("cap");
        let mut assembler = PdvAssembler {
            spool: Some(SpoolConfig {
                memory_threshold: 0,
                max_message_length: 8 * 1024 * 1024,
                dir: Some(dir.clone()),
            }),
            ..PdvAssembler::with_max_message_length(1024)
        };
        assembler.arm_spill(1);
        let unit = assembler
            .push_pdv(&dataset_pdv(1, true, 64 * 1024))
            .unwrap()
            .expect("one PDV completes the dataset");
        assert!(
            unit.spool.is_some(),
            "64 KiB is accepted although the memory cap is 1 KiB"
        );

        // the spool's own bound still applies
        let mut bounded = PdvAssembler::with_spool(SpoolConfig {
            memory_threshold: 0,
            max_message_length: 4,
            dir: Some(dir.clone()),
        });
        bounded.arm_spill(1);
        let error = bounded
            .push_pdv(&dataset_pdv(1, false, 16))
            .expect_err("the spool bound is enforced");
        assert!(error.to_string().contains("maximum size"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_small_armed_dataset_never_reaches_the_disk() {
        // with a threshold the small-instance path stays in memory (no temp
        // file for the common few-hundred-KB image)
        let dir = spill_dir("small");
        let mut assembler = PdvAssembler::with_spool(SpoolConfig {
            memory_threshold: 64 * 1024,
            max_message_length: 1024 * 1024,
            dir: Some(dir.clone()),
        });
        assembler.arm_spill(1);
        let mut completed = None;
        for index in 0..2 {
            let pdv = dataset_pdv(1, index == 1, 4096);
            completed = assembler.push_pdv(&pdv).unwrap().or(completed);
        }
        let unit = completed.expect("the last PDV completes the dataset");
        assert!(unit.spool.is_none(), "a small dataset stays in memory");
        assert_eq!(unit.bytes.len(), 8 * 1024);
        assert!(assembler.is_idle());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "no temp file was created"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn configured_pdv_limits_shrink_the_fragments_and_the_batch() {
        let splitter = PduSplitter::with_limits(512, 2, Some(64), Some(256));
        assert_eq!(splitter.max_command_pdv(), Some(64));
        assert_eq!(splitter.max_data_pdv(), Some(256));

        let command = splitter.split(1, true, &[0u8; 200]).unwrap();
        let values: Vec<usize> = command
            .iter()
            .flat_map(|pdu| pdu.pdvs.iter().map(|pdv| pdv.data.len()))
            .collect();
        assert_eq!(
            values,
            vec![64, 64, 64, 8],
            "MaxCommandBuffer caps each command PDV"
        );
        assert!(
            command.iter().all(|pdu| pdu.pdvs.len() <= 2),
            "MaxPDVsPerPDU caps the batch"
        );

        let dataset = splitter.split(1, false, &[0u8; 600]).unwrap();
        let values: Vec<usize> = dataset
            .iter()
            .flat_map(|pdu| pdu.pdvs.iter().map(|pdv| pdv.data.len()))
            .collect();
        assert_eq!(
            values,
            vec![256, 244, 100],
            "MaxDataBuffer caps each dataset PDV and the PDU still bounds the rest"
        );
        assert!(values.iter().all(|len| *len <= 256));

        // a configured cap may shrink the negotiated PDU but never raise it
        let capped = PduSplitter::with_limits(64, 4, Some(1 << 20), None);
        let pdus = capped.split(1, true, &[0u8; 200]).unwrap();
        assert!(pdus
            .iter()
            .flat_map(|pdu| pdu.pdvs.iter())
            .all(|pdv| pdv.data.len() <= 58));

        // the defaults keep the previous behaviour: one PDV fills the PDU
        let default = PduSplitter::new(512, 16);
        assert_eq!(default.split(1, true, &[0u8; 200]).unwrap().len(), 1);
    }

    #[test]
    fn message_body_reports_its_shape_without_buffering_a_spill() {
        // Memory variant: len/is_empty/in_memory/spool/read_all and From<Bytes>.
        let memory = MessageBody::from(bytes::Bytes::from_static(b"abcd"));
        assert_eq!(memory.len(), 4);
        assert!(!memory.is_empty());
        assert_eq!(memory.in_memory(), Some(&b"abcd"[..]));
        assert!(memory.spool().is_none());
        assert_eq!(
            memory.read_all().unwrap(),
            bytes::Bytes::from_static(b"abcd")
        );
        let empty = MessageBody::from(bytes::Bytes::new());
        assert!(empty.is_empty());

        // A brand-new SpoolFile has no bytes yet and compares by path.
        let dir = spill_dir("spoolfile");
        let first = SpoolFile::new(dir.join("a.dcm"));
        let same = SpoolFile::new(dir.join("a.dcm"));
        let other = SpoolFile::new(dir.join("b.dcm"));
        assert!(first.is_empty());
        assert_eq!(first.len(), 0);
        assert_eq!(first, same, "two handles to one path are equal");
        assert_ne!(first, other, "different paths are not equal");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_spilled_body_reads_back_from_its_spool_file() {
        let dir = spill_dir("body-read");
        let mut assembler = PdvAssembler::with_spool(SpoolConfig {
            memory_threshold: 0,
            dir: Some(dir.clone()),
            ..SpoolConfig::default()
        });
        assembler.arm_spill(1);
        let unit = assembler
            .push_pdv(&dataset_pdv(1, true, 8))
            .unwrap()
            .expect("the dataset completes");
        let body = unit.body();
        assert_eq!(body.len(), 8);
        assert!(!body.is_empty());
        assert!(body.in_memory().is_none(), "a spilled body left memory");
        assert!(body.spool().is_some(), "the spool file is exposed");
        assert_eq!(body.read_all().unwrap().len(), 8);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_spill_that_crosses_its_cap_mid_stream_is_dropped() {
        // The first PDV opens the file; a later PDV that would push it past
        // `max_message_length` is refused and the writer is discarded.
        let dir = spill_dir("spill-cap");
        let mut assembler = PdvAssembler::with_spool(SpoolConfig {
            memory_threshold: 4,
            max_message_length: 10,
            dir: Some(dir.clone()),
        });
        assembler.arm_spill(1);
        assert!(assembler
            .push_pdv(&dataset_pdv(1, false, 8))
            .unwrap()
            .is_none());
        let error = assembler
            .push_pdv(&dataset_pdv(1, false, 8))
            .expect_err("the second PDV crosses the spill cap");
        assert!(error.to_string().contains("maximum size"), "{error}");
        assert!(!assembler.is_spilling(1), "the failed spill is torn down");
        assert!(assembler.is_idle());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_memory_prefix_is_moved_into_the_spool_file() {
        // A small first PDV stays in memory; when a second one crosses the
        // threshold the buffered prefix is copied into the new temp file.
        let dir = spill_dir("prefix");
        let mut assembler = PdvAssembler::with_spool(SpoolConfig {
            memory_threshold: 8,
            max_message_length: 1024,
            dir: Some(dir.clone()),
        });
        assembler.arm_spill(1);
        assert!(assembler
            .push_pdv(&dataset_pdv(1, false, 4))
            .unwrap()
            .is_none());
        let unit = assembler
            .push_pdv(&dataset_pdv(1, true, 8))
            .unwrap()
            .expect("the last PDV completes the dataset");
        let spool = unit
            .spool
            .as_ref()
            .expect("it spilled once it passed the threshold");
        assert_eq!(spool.len(), 12, "the 4-byte prefix plus 8 bytes");
        let bytes = unit.body().read_all().unwrap();
        assert_eq!(bytes.len(), 12);
        std::fs::remove_dir_all(&dir).ok();
    }
}
