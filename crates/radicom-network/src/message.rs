//! DIMSE message encoding: command + dataset as one message (NET-05/06;
//! docs/01-architecture.md §11).
//!
//! A message is a command set followed by an optional dataset. The command
//! set is always Implicit VR Little Endian; the dataset uses the presentation
//! context's accepted transfer syntax.

use crate::error::{NetworkError, Result};
use crate::pdu::{PData, Pdv};
use radicom::io::dataset_builder::parse_dataset_bytes;
use radicom::io::writer::{DicomWriter, WriteOptions};
use radicom::model::dataset::Dataset;
use radicom::model::transfer_syntax::{syntaxes, TransferSyntax};

/// Encodes a command set, keeping its recomputed group length (§11.1).
pub fn encode_command(dataset: &Dataset) -> Result<Vec<u8>> {
    let options = WriteOptions {
        keep_group_lengths: true,
        ..Default::default()
    };
    let mut out = Vec::new();
    DicomWriter::new(options)
        .write_dataset(dataset, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN, &mut out)
        .map_err(|_| NetworkError::protocol("command set could not be encoded"))?;
    Ok(out)
}

/// Decodes a command set (§11.2).
pub fn decode_command(bytes: &[u8]) -> Result<Dataset> {
    parse_dataset_bytes(bytes, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN).map_err(|e| {
        NetworkError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    })
}

/// Encodes a dataset under the negotiated syntax, dropping group lengths (§11.1).
pub fn encode_dataset(dataset: &Dataset, syntax: &TransferSyntax) -> Result<Vec<u8>> {
    let options = WriteOptions {
        keep_group_lengths: false,
        ..Default::default()
    };
    let mut out = Vec::new();
    DicomWriter::new(options)
        .write_dataset(dataset, syntax, &mut out)
        .map_err(|_| NetworkError::protocol("dataset could not be encoded"))?;
    Ok(out)
}

/// Decodes a dataset under the negotiated syntax (§11.2: validation off).
pub fn decode_dataset(bytes: &[u8], syntax: &TransferSyntax) -> Result<Dataset> {
    parse_dataset_bytes(bytes, syntax).map_err(|e| {
        NetworkError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    })
}

/// Splits a command (and optional dataset) into P-DATA-TF PDUs (§11.1).
///
/// The command stream always occupies its own P-DATA-TF, before the dataset.
pub fn split_message(
    context_id: u8,
    command: &[u8],
    dataset: Option<&[u8]>,
    max_pdu: u32,
    max_pdvs_per_pdu: usize,
) -> Result<Vec<PData>> {
    let splitter = crate::assembler::PduSplitter::new(max_pdu, max_pdvs_per_pdu);
    splitter.split_message(context_id, command, dataset)
}

/// True when a PDV sequence carries a command first, as required by §5.3.
pub fn starts_with_command(pdus: &[PData]) -> bool {
    pdus.first()
        .and_then(|p| p.pdvs.first())
        .map(|pdv: &Pdv| pdv.is_command)
        .unwrap_or(false)
}

/// Frames a byte stream into P-DATA-TF PDUs as it is written (NET-13).
///
/// Each PDU carries one PDV, so the sink holds at most one PDU's payload plus the
/// chunk being appended: a multi-GB dataset streams in O(PDU) memory instead of
/// being encoded into one `Vec<u8>`. The tail is held back and emitted by
/// [`Self::finish`] so the final PDV can carry the "last" bit (§11.1).
///
/// ponytail: one PDV per PDU (the buffered splitter packs several) — slightly
/// more PDUs on the wire for the same bytes; pack harder only if a peer objects.
pub struct PduFramer<F: FnMut(bytes::Bytes) -> std::io::Result<()>> {
    context_id: u8,
    /// largest PDV value that fits one PDU (even, per review #17)
    room: usize,
    pending: Vec<u8>,
    emit: F,
    max_buffered: usize,
    finished: bool,
}

impl<F: FnMut(bytes::Bytes) -> std::io::Result<()>> PduFramer<F> {
    /// `max_pdv_value` comes from [`crate::assembler::PduSplitter::max_pdv_value`].
    ///
    /// `room` is forced even: a PDV value is even-padded on the wire and the
    /// receiver keeps the pad byte, so an odd `room` would splice pad bytes into
    /// the dataset and corrupt it.
    pub fn new(context_id: u8, max_pdv_value: usize, emit: F) -> Self {
        let room = (max_pdv_value & !1).max(2);
        debug_assert_eq!(room % 2, 0, "a PDV value must be even");
        PduFramer {
            context_id,
            room,
            pending: Vec::with_capacity(room),
            emit,
            max_buffered: 0,
            finished: false,
        }
    }

    /// The most payload bytes this framer ever held: the memory bound the
    /// streamed path guarantees (never more than one PDV value).
    pub fn max_buffered(&self) -> usize {
        self.max_buffered
    }

    fn emit_pdu(&mut self, data: bytes::Bytes, is_last: bool) -> std::io::Result<()> {
        let pdu = crate::pdu::Pdu::PData(PData {
            pdvs: vec![Pdv {
                context_id: self.context_id,
                is_command: false,
                is_last,
                data,
            }],
        });
        let mut encoded = Vec::new();
        crate::pdu::encode_into(&pdu, &mut encoded)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        (self.emit)(encoded.into())
    }

    fn push(&mut self, mut input: &[u8]) -> std::io::Result<()> {
        while !input.is_empty() {
            if self.pending.len() == self.room {
                let full = std::mem::take(&mut self.pending);
                self.pending = Vec::with_capacity(self.room);
                self.emit_pdu(full.into(), false)?;
            }
            let take = (self.room - self.pending.len()).min(input.len());
            self.pending.extend_from_slice(&input[..take]);
            self.max_buffered = self.max_buffered.max(self.pending.len());
            input = &input[take..];
        }
        Ok(())
    }

    /// Emits the held tail as the last PDU of the message (§11.1). Idempotent.
    pub fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let tail = std::mem::take(&mut self.pending);
        self.emit_pdu(tail.into(), true)
    }
}

impl<F: FnMut(bytes::Bytes) -> std::io::Result<()>> std::io::Write for PduFramer<F> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.push(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Sends a command and a dataset as one DIMSE message, streaming the dataset
/// (NET-13). The command goes out first in its own P-DATA-TF; the dataset is then
/// encoded on a blocking thread into a bounded channel that this task writes out.
///
/// Peak memory is O(max_pdu + large_object_size + a few PDUs), independent of the
/// instance size: the framer holds at most one PDV value, the channel holds two
/// encoded PDUs, and the writer's own scratch buffer is `large_object_size`.
///
/// `dataset` is moved so the encoder can run off-thread; a `Dataset` clone is
/// cheap because its buffers are shared (`Arc`).
///
/// On any failure once the command PDUs are on the wire the peer holds a partial
/// message, so this sends A-ABORT (§9.3.4) and returns a fatal error: the
/// association must be dropped, not reused.
pub async fn write_message_streamed<W>(
    stream: &mut W,
    context_id: u8,
    command: bytes::Bytes,
    dataset: Dataset,
    syntax: std::sync::Arc<TransferSyntax>,
    max_pdu: u32,
    max_pdvs_per_pdu: usize,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let result = write_streamed_inner(
        stream,
        context_id,
        command,
        dataset,
        syntax,
        max_pdu,
        max_pdvs_per_pdu,
    )
    .await;
    if result.is_err() {
        // Best effort: the stream may already be broken, which is why we abort.
        let _ = abort(stream).await;
    }
    result
}

/// Encodes the command, then streams the dataset; see [`write_message_streamed`].
async fn write_streamed_inner<W>(
    stream: &mut W,
    context_id: u8,
    command: bytes::Bytes,
    dataset: Dataset,
    syntax: std::sync::Arc<TransferSyntax>,
    max_pdu: u32,
    max_pdvs_per_pdu: usize,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;

    let splitter = crate::assembler::PduSplitter::new(max_pdu, max_pdvs_per_pdu);
    // the command always fits one PDU in practice, but stay within the limit
    for pdu in splitter.split_message_bytes(context_id, command, None)? {
        let mut bytes = Vec::new();
        crate::pdu::encode_into(&crate::pdu::Pdu::PData(pdu), &mut bytes)?;
        stream.write_all(&bytes).await.map_err(NetworkError::Io)?;
    }

    let pdv_value = splitter.max_pdv_value();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(2);
    let encoder = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut framer = PduFramer::new(context_id, pdv_value, |frame: bytes::Bytes| {
            tx.blocking_send(frame)
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        });
        let options = WriteOptions {
            keep_group_lengths: false,
            ..Default::default()
        };
        DicomWriter::new(options)
            .write_dataset(&dataset, &syntax, &mut framer)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        framer.finish()
    });

    while let Some(frame) = rx.recv().await {
        stream.write_all(&frame).await.map_err(NetworkError::Io)?;
    }
    stream.flush().await.map_err(NetworkError::Io)?;
    match encoder.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(NetworkError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("dataset could not be encoded: {error}"),
        ))),
        Err(join) => Err(NetworkError::Io(std::io::Error::other(format!(
            "dataset encoder task failed: {join}"
        )))),
    }
}

/// Sends A-ABORT (source: service-user) so a peer that saw a partial message
/// discards it instead of waiting for the rest (§9.3.4).
async fn abort<W>(stream: &mut W) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;
    let abort = crate::pdu::Abort {
        source: crate::error::AbortSource::Provider,
        reason: 0,
    };
    let mut frame = Vec::new();
    crate::pdu::encode_into(&crate::pdu::Pdu::Abort(abort), &mut frame)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    stream.write_all(&frame).await?;
    stream.flush().await
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dimse::{Command, CommandField};
    use radicom::model::tag::Tag;
    use radicom::model::vr::Vr;

    fn sample_dataset() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds
    }

    #[test]
    fn the_framer_holds_at_most_one_pdu_and_marks_the_last_pdv() {
        use std::io::Write as _;
        // an odd max_pdv_value must be rounded down to an even room
        let room = 100usize;
        let mut frames: Vec<bytes::Bytes> = Vec::new();
        let mut framer = PduFramer::new(3, room, |frame| {
            frames.push(frame);
            Ok(())
        });
        let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        // odd chunk sizes, so a chunk straddles PDUs
        for chunk in payload.chunks(37) {
            framer.write_all(chunk).unwrap();
        }
        framer.finish().unwrap();

        assert!(
            framer.max_buffered() <= room,
            "held {} bytes, more than one PDV value ({room})",
            framer.max_buffered()
        );

        let mut joined = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            // 6-byte PDU header + 6-byte PDV header + the value
            assert!(
                frame.len() <= room + 12,
                "a frame ({}) exceeded one PDU ({} max)",
                frame.len(),
                room + 12
            );
            let crate::pdu::Pdu::PData(pdata) = crate::pdu::decode(frame).unwrap() else {
                panic!("the framer emits only P-DATA")
            };
            for pdv in &pdata.pdvs {
                assert!(!pdv.is_command);
                assert_eq!(pdv.context_id, 3);
                assert_eq!(
                    pdv.is_last,
                    index + 1 == frames.len(),
                    "only the final PDV is marked last"
                );
                assert!(
                    pdv.data.len() % 2 == 0,
                    "a PDV value must stay even so no pad byte is spliced in"
                );
                joined.extend_from_slice(&pdv.data);
            }
        }
        assert_eq!(joined, payload, "the framed bytes reassemble");
        assert!(frames.len() > 5, "the payload really was split");
    }

    #[test]
    fn an_odd_max_pdv_value_is_rounded_down_to_an_even_room() {
        use std::io::Write as _;
        let mut frames: Vec<bytes::Bytes> = Vec::new();
        let mut framer = PduFramer::new(1, 101, |frame| {
            frames.push(frame);
            Ok(())
        });
        framer.write_all(&[0xABu8; 400]).unwrap();
        framer.finish().unwrap();
        assert_eq!(framer.max_buffered(), 100, "101 rounds down to an even 100");
        for frame in &frames {
            assert!(frame.len() <= 100 + 12);
        }
    }

    /// An `AsyncWrite` that fails once, after `fail_at` bytes, then accepts
    /// everything (so the A-ABORT that follows the failure is still recorded).
    struct FailOnce {
        buf: Vec<u8>,
        fail_at: usize,
        failed: bool,
    }

    impl tokio::io::AsyncWrite for FailOnce {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            data: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            if !this.failed && this.buf.len() >= this.fail_at {
                this.failed = true;
                return std::task::Poll::Ready(Err(std::io::Error::other("boom")));
            }
            this.buf.extend_from_slice(data);
            std::task::Poll::Ready(Ok(data.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_mid_stream_failure_sends_an_abort() {
        let mut sink = FailOnce {
            buf: Vec::new(),
            fail_at: 1,
            failed: false,
        };
        let error = write_message_streamed(
            &mut sink,
            1,
            bytes::Bytes::from_static(b"cmd"),
            sample_dataset(),
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            256,
            16,
        )
        .await;
        assert!(error.is_err(), "a write failure is reported");

        let mut last = None;
        let mut offset = 0usize;
        while offset + 6 <= sink.buf.len() {
            let len = u32::from_be_bytes([
                sink.buf[offset + 2],
                sink.buf[offset + 3],
                sink.buf[offset + 4],
                sink.buf[offset + 5],
            ]) as usize;
            let end = offset + 6 + len;
            last = Some(crate::pdu::decode(&sink.buf[offset..end]).unwrap());
            offset = end;
        }
        assert!(
            matches!(last, Some(crate::pdu::Pdu::Abort(_))),
            "a partial message is followed by A-ABORT, got {last:?}"
        );
    }

    #[test]
    fn an_empty_dataset_still_emits_one_last_pdv() {
        let mut frames: Vec<bytes::Bytes> = Vec::new();
        let mut framer = PduFramer::new(2, 64, |frame| {
            frames.push(frame);
            Ok(())
        });
        framer.finish().unwrap();
        assert_eq!(frames.len(), 1, "even an empty message needs a last PDV");
        let crate::pdu::Pdu::PData(pdata) = crate::pdu::decode(&frames[0]).unwrap() else {
            panic!("expected P-DATA")
        };
        assert_eq!(pdata.pdvs.len(), 1);
        assert!(pdata.pdvs[0].is_last);
        assert!(pdata.pdvs[0].data.is_empty());
    }

    #[tokio::test]
    async fn a_streamed_message_reassembles_to_the_buffered_message() {
        use crate::assembler::PdvAssembler;
        use tokio::io::AsyncWriteExt as _;

        let dataset = sample_dataset();
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let command = bytes::Bytes::from_static(b"command-bytes");

        // a tiny duplex buffer forces backpressure through the bounded channel
        let (mut client, mut server) = tokio::io::duplex(64);
        let reader = tokio::spawn(async move {
            let mut all = Vec::new();
            let mut buf = [0u8; 64];
            loop {
                match tokio::io::AsyncReadExt::read(&mut server, &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => all.extend_from_slice(&buf[..n]),
                }
            }
            all
        });
        write_message_streamed(
            &mut client,
            5,
            command.clone(),
            dataset.clone(),
            syntax.clone(),
            128,
            1,
        )
        .await
        .unwrap();
        client.shutdown().await.unwrap();
        let wire = reader.await.unwrap();

        // split the exchanged frames back into message units
        let units_of = |frames: Vec<bytes::Bytes>| {
            let mut assembler = PdvAssembler::new();
            let mut command_bytes = Vec::new();
            let mut dataset_bytes = Vec::new();
            for frame in frames {
                let crate::pdu::Pdu::PData(pdata) = crate::pdu::decode(&frame).unwrap() else {
                    continue;
                };
                for unit in assembler.push_pdata(&pdata).unwrap() {
                    if unit.stream == crate::assembler::Stream::Command {
                        command_bytes.extend_from_slice(&unit.bytes);
                    } else {
                        dataset_bytes.extend_from_slice(&unit.bytes);
                    }
                }
            }
            (command_bytes, dataset_bytes)
        };

        let mut streamed_frames = Vec::new();
        let mut offset = 0usize;
        while offset + 6 <= wire.len() {
            let len = u32::from_be_bytes([
                wire[offset + 2],
                wire[offset + 3],
                wire[offset + 4],
                wire[offset + 5],
            ]) as usize;
            let end = offset + 6 + len;
            assert!(
                end - offset <= 128,
                "a streamed frame ({}) exceeded the negotiated max_pdu (128)",
                end - offset
            );
            streamed_frames.push(bytes::Bytes::copy_from_slice(&wire[offset..end]));
            offset = end;
        }
        let streamed = units_of(streamed_frames);

        // the buffered splitter is the reference
        let encoded = encode_dataset(&dataset, &syntax).unwrap();
        let buffered = units_of(
            split_message(5, &command, Some(&encoded), 128, 1)
                .unwrap()
                .into_iter()
                .map(|pdu| {
                    bytes::Bytes::from(crate::pdu::encode(&crate::pdu::Pdu::PData(pdu)).unwrap())
                })
                .collect(),
        );

        assert_eq!(streamed.0, buffered.0, "the command bytes are unchanged");
        assert_eq!(
            streamed.1, buffered.1,
            "the streamed dataset reassembles to the buffered bytes"
        );
    }

    #[tokio::test]
    async fn a_large_dataset_streams_through_many_pdus_intact() {
        use crate::assembler::{PdvAssembler, Stream};
        use radicom::model::buffer::MemoryBuffer;
        use radicom::model::element::DicomElement;

        // 2 MiB of pixel data: far more than one PDU at max_pdu = 1024
        let mut dataset = sample_dataset();
        dataset.insert_or_update(radicom::model::item::DicomItem::Element(
            DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(MemoryBuffer::from(vec![0x5Au8; 2 * 1024 * 1024])),
            ),
        ));
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let command = bytes::Bytes::from_static(b"cmd");

        let (mut client, mut server) = tokio::io::duplex(4096);
        let reader = tokio::spawn(async move {
            let mut all = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match tokio::io::AsyncReadExt::read(&mut server, &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => all.extend_from_slice(&buf[..n]),
                }
            }
            all
        });
        write_message_streamed(
            &mut client,
            7,
            command,
            dataset.clone(),
            syntax.clone(),
            1024,
            1,
        )
        .await
        .unwrap();
        tokio::io::AsyncWriteExt::shutdown(&mut client)
            .await
            .unwrap();
        let wire = reader.await.unwrap();

        let mut assembler = PdvAssembler::new();
        let mut dataset_bytes = Vec::new();
        let mut offset = 0usize;
        while offset + 6 <= wire.len() {
            let len = u32::from_be_bytes([
                wire[offset + 2],
                wire[offset + 3],
                wire[offset + 4],
                wire[offset + 5],
            ]) as usize;
            let frame = bytes::Bytes::copy_from_slice(&wire[offset..offset + 6 + len]);
            offset += 6 + len;
            let crate::pdu::Pdu::PData(pdata) = crate::pdu::decode(&frame).unwrap() else {
                continue;
            };
            for unit in assembler.push_pdata(&pdata).unwrap() {
                if unit.stream == Stream::Data {
                    dataset_bytes.extend_from_slice(&unit.bytes);
                }
            }
        }
        assert_eq!(
            dataset_bytes,
            encode_dataset(&dataset, &syntax).unwrap(),
            "2 MiB survives many PDUs byte-for-byte"
        );
    }

    #[test]
    fn command_encoding_keeps_the_group_length() {
        let command = Command::request(CommandField::CEchoRq, 1);
        let bytes = encode_command(&command.to_dataset()).unwrap();
        // (0000,0000) is the first element and must be present
        assert_eq!(&bytes[..4], &[0x00, 0x00, 0x00, 0x00]);
        let ds = decode_command(&bytes).unwrap();
        assert!(
            ds.contains(&Tag::COMMAND_GROUP_LENGTH),
            "command group length kept"
        );
        assert_eq!(
            Command::from_dataset(&ds).unwrap().field,
            CommandField::CEchoRq
        );
    }

    #[test]
    fn dataset_round_trips_under_each_syntax() {
        let ds = sample_dataset();
        for syntax in [
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone(),
        ] {
            let bytes = encode_dataset(&ds, &syntax).unwrap();
            let back = decode_dataset(&bytes, &syntax).unwrap();
            assert_eq!(back, ds, "round-trip under {}", syntax.uid());
        }
    }

    #[test]
    fn dataset_encoding_drops_group_lengths() {
        let mut ds = sample_dataset();
        ds.insert_value(Tag::new(0x0010, 0x0000), 4u32);
        let bytes = encode_dataset(&ds, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
        let back = decode_dataset(&bytes, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
        assert!(
            !back.contains(&Tag::new(0x0010, 0x0000)),
            "group lengths removed on send (§11.1)"
        );
    }

    #[test]
    fn message_splitting_puts_the_command_first() {
        let command =
            encode_command(&Command::request(CommandField::CStoreRq, 3).to_dataset()).unwrap();
        let dataset =
            encode_dataset(&sample_dataset(), &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
        let pdus = split_message(1, &command, Some(&dataset), 16384, 16).unwrap();
        assert_eq!(pdus.len(), 2);
        assert!(starts_with_command(&pdus));
        assert!(pdus[0].pdvs.iter().all(|p| p.is_command));
        assert!(pdus[1].pdvs.iter().all(|p| !p.is_command));
    }

    #[test]
    fn command_only_message_has_one_pdu_set() {
        let command =
            encode_command(&Command::request(CommandField::CEchoRq, 1).to_dataset()).unwrap();
        let pdus = split_message(1, &command, None, 16384, 16).unwrap();
        assert_eq!(pdus.len(), 1);
        assert!(starts_with_command(&pdus));
    }

    #[test]
    fn reassembly_of_a_split_message_is_lossless() {
        let command =
            encode_command(&Command::request(CommandField::CStoreRq, 9).to_dataset()).unwrap();
        let dataset =
            encode_dataset(&sample_dataset(), &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
        // a small maximum forces several PDVs per stream
        let pdus = split_message(1, &command, Some(&dataset), 48, 4).unwrap();
        let mut assembler = crate::assembler::PdvAssembler::new();
        let mut units = Vec::new();
        for pdu in pdus {
            units.extend(assembler.push_pdata(&pdu).unwrap());
        }
        assert_eq!(units.len(), 2, "one command unit and one dataset unit");
        assert_eq!(units[0].bytes, command);
        assert_eq!(units[1].bytes, dataset);
    }
}
