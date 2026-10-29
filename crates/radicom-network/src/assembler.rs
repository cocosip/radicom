//! PDV assembly and PDU fragmentation (NET-05/06; docs/01-architecture.md §11).
//!
//! Both directions are pure state machines over byte slices, so the wire
//! behaviour (fragmenting, even padding, command/dataset splitting) is fully
//! unit-testable without a socket.

use crate::error::{NetworkError, Result};
use crate::pdu::{PData, Pdv};

/// Which stream a PDV belongs to (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Command,
    Data,
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
#[derive(Debug, Clone)]
pub struct PdvAssembler {
    /// Half-assembled message per `(context id, stream)`.
    buffers: std::collections::HashMap<(u8, Stream), bytes::BytesMut>,
    /// Largest single message this assembler accepts, over all streams.
    max_message_length: usize,
}

impl Default for PdvAssembler {
    fn default() -> Self {
        PdvAssembler {
            buffers: std::collections::HashMap::new(),
            max_message_length: Self::DEFAULT_MAX_MESSAGE_LENGTH,
        }
    }
}

/// A completed message unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageUnit {
    pub context_id: u8,
    pub stream: Stream,
    /// The message bytes. Single-PDV messages share the PDV's `Bytes` (no copy);
    /// fragmented ones are frozen from a `BytesMut` exactly once.
    pub bytes: bytes::Bytes,
}

impl PdvAssembler {
    /// Largest message a default assembler will buffer (1 GiB).
    ///
    /// A message is buffered whole before it is dispatched, so this is also the
    /// ceiling on one C-STORE dataset: an instance above it is refused with a
    /// protocol error rather than streamed to the sink. Receiving multi-GB
    /// instances needs a streaming sink (docs/06-backlog.md §1, NET-13).
    pub const DEFAULT_MAX_MESSAGE_LENGTH: usize = 1 << 30;

    pub fn new() -> Self {
        Self::default()
    }

    /// An assembler that rejects messages larger than `max_message_length`.
    pub fn with_max_message_length(max_message_length: usize) -> Self {
        PdvAssembler {
            max_message_length,
            ..Self::default()
        }
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
            }));
        }
        let buffered = self.buffers.get(&key).map_or(0, bytes::BytesMut::len);
        if buffered + pdv.data.len() > self.max_message_length {
            // drop the partial message: continuing would only buffer more
            self.buffers.remove(&key);
            return Err(NetworkError::protocol(
                "DIMSE message exceeds the maximum size",
            ));
        }
        let buffer = self.buffers.entry(key).or_default();
        buffer.extend_from_slice(&pdv.data);
        if !pdv.is_last {
            return Ok(None);
        }

        // the message is complete: drop the buffer so idle contexts cost nothing
        let bytes = self.buffers.remove(&key).unwrap_or_default().freeze();
        Ok(Some(MessageUnit {
            context_id: pdv.context_id,
            stream,
            bytes,
        }))
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
}

impl PduSplitter {
    /// `max_pdu` is the peer's maximum PDU length; `max_pdvs_per_pdu` exists
    /// for buggy peers that only accept one PDV per PDU.
    pub fn new(max_pdu: u32, max_pdvs_per_pdu: usize) -> Self {
        PduSplitter {
            max_pdu,
            max_pdvs_per_pdu: max_pdvs_per_pdu.max(1),
        }
    }

    /// The largest PDV value that fits in one PDU.
    ///
    /// The negotiated maximum counts the whole PDU including its 6-byte header
    /// (as fo-dicom's `CurrentPduSize`, `DicomService.cs:1869-1881`), and each
    /// PDV costs another 6 header bytes. PDV values are even-padded on the
    /// wire, so the usable size is even too: an odd maximum must not become an
    /// odd fragment plus a pad byte (review #17).
    pub fn max_pdv_value(&self) -> usize {
        ((self.max_pdu as usize).saturating_sub(12)) & !1
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
    /// The negotiated maximum covers the whole PDU: 6 bytes of PDU header plus
    /// the body (fo-dicom `DicomService.cs:1813-1881`, PS3.8 §9.3.2).
    pub fn split_bytes(
        &self,
        context_id: u8,
        is_command: bool,
        data: bytes::Bytes,
    ) -> Result<Vec<PData>> {
        let body_limit = (self.max_pdu as usize).saturating_sub(6);
        // 6 bytes for the PDV header plus a non-empty even-padded value
        if body_limit < 12 {
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
            let room = (available - 6) & !1;
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
        // max PDU 24 is the whole frame: 6 header bytes leave 18 of body, and
        // the PDV header takes 6 more, so each fragment carries 12 bytes.
        let splitter = PduSplitter::new(24, 16);
        assert_eq!(splitter.max_pdv_value(), 12);
        let data = vec![0xAB; 40];
        let pdus = splitter.split(1, true, &data).unwrap();
        assert_eq!(pdus.len(), 4, "each PDV fills its PDU");
        assert_eq!(
            pdus.iter()
                .map(|p| p.pdvs[0].data.len())
                .collect::<Vec<_>>(),
            vec![12, 12, 12, 4]
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
        assert_eq!(last_flags, vec![false, false, false, true]);
    }

    #[test]
    fn a_pdu_never_exceeds_the_negotiated_maximum() {
        // NET-03: the negotiated maximum counts the whole PDU, 6-byte header
        // included (fo-dicom `CurrentPduSize`, DicomService.cs:1869-1881).
        let splitter = PduSplitter::new(16384, 16);
        let input: Vec<u8> = (0..(1 << 20)).map(|i| (i % 251) as u8).collect();
        let pdus = splitter.split(1, false, &input).unwrap();
        assert!(pdus.len() > 1, "the message is fragmented");
        let mut largest = 0;
        let mut joined = Vec::new();
        for pdu in pdus {
            let wire = crate::pdu::encode(&crate::pdu::Pdu::PData(pdu)).unwrap();
            assert!(
                wire.len() <= 16384,
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
        assert_eq!(pdus.len(), 4, "one 12-byte PDV per PDU");
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
                wire.len() <= 16385,
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
}
