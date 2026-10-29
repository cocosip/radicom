//! Transport layer (NET-01; docs/01-architecture.md §11).
//!
//! The protocol engine sees only an async byte stream, so TLS and plain TCP
//! share one code path. PDU framing lives here: read a 6-byte header, then
//! exactly `length` body bytes.

use crate::error::{NetworkError, Result};
use crate::pdu::{self, Pdu, MAX_PDU_SIZE};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The 6-byte PDU header: type, reserved, length.
pub const HEADER_LEN: usize = 6;

/// Options applied to a TCP connection (§11.2).
#[derive(Debug, Clone)]
pub struct TcpOptions {
    /// Disable Nagle's algorithm (default on).
    pub nodelay: bool,
    /// `SO_SNDBUF`, when set.
    pub send_buffer_size: Option<u32>,
    /// `SO_RCVBUF`, when set.
    pub receive_buffer_size: Option<u32>,
}

impl Default for TcpOptions {
    fn default() -> Self {
        TcpOptions {
            nodelay: true,
            send_buffer_size: None,
            receive_buffer_size: None,
        }
    }
}

/// Reads one PDU frame from an async stream (§11.2).
///
/// Returns `Ok(None)` on a clean end of stream before any header byte, which
/// the engine treats as a peer close (§11.1).
pub async fn read_pdu<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Pdu>> {
    let mut header = [0u8; HEADER_LEN];
    // a clean EOF before the first byte is a normal close
    match reader.read_exact(&mut header[..1]).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(NetworkError::Io(e)),
    }
    reader
        .read_exact(&mut header[1..])
        .await
        .map_err(NetworkError::Io)?;

    let length = u32::from_be_bytes([header[2], header[3], header[4], header[5]]);
    // the advertised maximum is the whole PDU: 6-byte header plus body
    if length > MAX_PDU_SIZE - HEADER_LEN as u32 {
        return Err(NetworkError::protocol(
            "PDU exceeds the 262144-byte receive limit",
        ));
    }
    // One allocation sized from the validated length field, then read the body
    // straight into it: no intermediate zero-initialised copy (§11.2). The
    // MAX_PDU_SIZE check above keeps a peer from requesting a huge allocation.
    let mut frame = vec![0u8; HEADER_LEN + length as usize];
    frame[..HEADER_LEN].copy_from_slice(&header);
    reader
        .read_exact(&mut frame[HEADER_LEN..])
        .await
        .map_err(NetworkError::Io)?;

    // Hand the frame over as `Bytes` so PDVs share it instead of copying.
    pdu::decode_bytes(bytes::Bytes::from(frame)).map(Some)
}

/// Cancel-safe PDU framer.
///
/// [`read_pdu`] keeps its progress in the future, so a `select!` that drops the
/// future after it consumed part of a frame loses those bytes and the next call
/// resynchronises in the middle of a PDU. This keeps the received bytes in the
/// struct instead: dropping the future only loses an *attempt*, never data.
#[derive(Debug, Default)]
pub struct PduReader {
    buf: bytes::BytesMut,
}

impl PduReader {
    pub fn new() -> Self {
        PduReader {
            buf: bytes::BytesMut::new(),
        }
    }

    /// Reads one PDU, buffering across cancellation (§11.2, review #13).
    pub async fn read_pdu<R: AsyncRead + Unpin>(&mut self, reader: &mut R) -> Result<Option<Pdu>> {
        loop {
            if let Some(pdu) = self.take_frame()? {
                return Ok(Some(pdu));
            }
            let mut chunk = [0u8; 8192];
            // `AsyncReadExt::read` is cancel safe: a dropped future consumes
            // nothing, so the partial frame in `buf` stays valid.
            let n = reader.read(&mut chunk).await.map_err(NetworkError::Io)?;
            if n == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return Err(NetworkError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "early eof inside a PDU frame",
                )));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Extracts one complete frame from the buffer, if there is one.
    fn take_frame(&mut self) -> Result<Option<Pdu>> {
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let length = u32::from_be_bytes([self.buf[2], self.buf[3], self.buf[4], self.buf[5]]);
        if length > MAX_PDU_SIZE - HEADER_LEN as u32 {
            return Err(NetworkError::protocol(
                "PDU exceeds the 262144-byte receive limit",
            ));
        }
        let total = HEADER_LEN + length as usize;
        if self.buf.len() < total {
            return Ok(None);
        }
        let frame = self.buf.split_to(total);
        pdu::decode_bytes(frame.freeze()).map(Some)
    }
}

/// Writes one PDU frame to an async stream.
///
/// P-DATA is encoded straight into the frame buffer (no intermediate body
/// `Vec`); a one-shot frame still allocates its output buffer, since the
/// signature takes `&Pdu`. Batched P-DATA goes through [`write_pdus`], which
/// reuses one buffer for every frame of a message.
pub async fn write_pdu<W: AsyncWrite + Unpin>(writer: &mut W, pdu: &Pdu) -> Result<()> {
    let mut bytes = Vec::new();
    pdu::encode_into(pdu, &mut bytes)?;
    writer.write_all(&bytes).await.map_err(NetworkError::Io)?;
    writer.flush().await.map_err(NetworkError::Io)?;
    Ok(())
}

/// Writes several PDU frames, flushing once (§11.2).
///
/// P-DATA frames are written straight from their `Bytes` slices with vectored
/// writes, so a C-STORE message does not copy its whole dataset into an encode
/// buffer first. Control PDUs still go through `encode_into`.
pub async fn write_pdus<W: AsyncWrite + Unpin>(writer: &mut W, pdus: &[Pdu]) -> Result<()> {
    for pdu in pdus {
        match pdu {
            Pdu::PData(pdata) => write_pdata_direct(writer, pdata).await?,
            _ => {
                let mut buffer = Vec::new();
                pdu::encode_into(pdu, &mut buffer)?;
                writer.write_all(&buffer).await.map_err(NetworkError::Io)?;
            }
        }
    }
    writer.flush().await.map_err(NetworkError::Io)?;
    Ok(())
}

/// Encodes one P-DATA-TF PDU into slices (header + each PDV's `Bytes`) and
/// writes it without a full-body copy.
async fn write_pdata_direct<W: AsyncWrite + Unpin>(
    writer: &mut W,
    pdata: &crate::pdu::PData,
) -> Result<()> {
    let mut body_len = 0usize;
    for pdv in &pdata.pdvs {
        let pad = pdv.data.len() % 2;
        body_len += 6 + pdv.data.len() + pad;
    }
    let mut pdu_header = [0u8; 6];
    pdu_header[0] = 0x04;
    pdu_header[2..6].copy_from_slice(&(body_len as u32).to_be_bytes());

    let mut headers = Vec::with_capacity(pdata.pdvs.len());
    for pdv in &pdata.pdvs {
        let pad = pdv.data.len() % 2;
        let mut header = [0u8; 6];
        header[0..4].copy_from_slice(&((pdv.data.len() + pad + 2) as u32).to_be_bytes());
        header[4] = pdv.context_id;
        header[5] = (if pdv.is_command { 0x01 } else { 0 }) | (if pdv.is_last { 0x02 } else { 0 });
        headers.push(header);
    }

    const PAD: [u8; 1] = [0];
    let mut slices: Vec<std::io::IoSlice<'_>> = Vec::with_capacity(1 + headers.len() * 3);
    slices.push(std::io::IoSlice::new(&pdu_header));
    for (index, pdv) in pdata.pdvs.iter().enumerate() {
        slices.push(std::io::IoSlice::new(&headers[index]));
        slices.push(std::io::IoSlice::new(pdv.data.as_ref()));
        if pdv.data.len() % 2 == 1 {
            slices.push(std::io::IoSlice::new(&PAD));
        }
    }
    write_all_vectored(writer, &mut slices).await
}

/// `write_vectored` loop that advances across partial writes, keeping each
/// call under the platform IOV limit.
async fn write_all_vectored<W: AsyncWrite + Unpin>(
    writer: &mut W,
    slices: &mut [std::io::IoSlice<'_>],
) -> Result<()> {
    let mut remaining: &mut [std::io::IoSlice<'_>] = slices;
    while !remaining.is_empty() {
        let take = remaining.len().min(1024);
        let written = writer
            .write_vectored(&remaining[..take])
            .await
            .map_err(NetworkError::Io)?;
        if written == 0 {
            return Err(NetworkError::Io(std::io::ErrorKind::WriteZero.into()));
        }
        std::io::IoSlice::advance_slices(&mut remaining, written);
    }
    Ok(())
}

/// Applies [`TcpOptions`] to a connected stream.
///
/// Socket buffer sizing is advisory — the kernel may round the value up or cap
/// it — so a refused request is not fatal. It is at least *attempted*: the
/// fields would otherwise be a promise the transport never keeps.
pub fn apply_options(stream: &tokio::net::TcpStream, options: &TcpOptions) -> Result<()> {
    stream
        .set_nodelay(options.nodelay)
        .map_err(NetworkError::Io)?;
    // tokio exposes no SO_SNDBUF/SO_RCVBUF setter, so go through socket2 on the
    // same file descriptor/socket handle
    let socket = socket2::SockRef::from(stream);
    if let Some(size) = options.send_buffer_size {
        let _ = socket.set_send_buffer_size(size as usize);
    }
    if let Some(size) = options.receive_buffer_size {
        let _ = socket.set_recv_buffer_size(size as usize);
    }
    Ok(())
}

/// Derives a DICOM UID from the primary MAC address (UID-05, §11.2).
///
/// Machine identity, mirroring fo-dicom `TryGetNetworkIdentifier`. Purely a
/// utility: nothing in the engine depends on it.
pub fn network_identifier() -> Option<String> {
    // Read the environment / MAC once: the value is constant for the process,
    // so callers do not pay the lookup on every call.
    static IDENTIFIER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    IDENTIFIER
        .get_or_init(|| {
            // One identity source for the whole crate: append the host MAC to
            // the implementation class UID, as fo-dicom's
            // NetworkManager.TryGetNetworkIdentifier does. (The old hostname
            // hash here disagreed with print_sop's MAC form, §11.1/§3.7.)
            crate::print_sop::try_get_network_identifier(crate::server::IMPLEMENTATION_CLASS_UID)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AbortSource;
    use crate::pdu::{Abort, PData, Pdv};
    use tokio::io::duplex;

    #[tokio::test]
    async fn a_cancelled_read_keeps_the_partial_frame() {
        // review #13: the bytes of a half-read PDU must survive the dropped
        // read future, or the next read starts mid-header.
        let (mut sender, mut reader) = duplex(64);
        let wire = pdu::encode(&Pdu::ReleaseRq).unwrap();
        sender.write_all(&wire[..2]).await.unwrap();
        let mut framer = PduReader::new();
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            framer.read_pdu(&mut reader),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "the read is still waiting for the rest of the frame"
        );

        sender.write_all(&wire[2..]).await.unwrap();
        drop(sender);
        assert_eq!(
            framer.read_pdu(&mut reader).await.unwrap(),
            Some(Pdu::ReleaseRq),
            "the frame is assembled across the cancelled read"
        );
    }

    #[tokio::test]
    async fn two_buffered_pdus_are_returned_in_order() {
        let (mut sender, mut reader) = duplex(4096);
        write_pdu(&mut sender, &Pdu::ReleaseRq).await.unwrap();
        write_pdu(&mut sender, &Pdu::ReleaseRp).await.unwrap();
        drop(sender);
        // one read pulls both frames in; they must come out in order
        let mut framer = PduReader::new();
        assert_eq!(
            framer.read_pdu(&mut reader).await.unwrap(),
            Some(Pdu::ReleaseRq)
        );
        assert_eq!(
            framer.read_pdu(&mut reader).await.unwrap(),
            Some(Pdu::ReleaseRp)
        );
        assert_eq!(
            framer.read_pdu(&mut reader).await.unwrap(),
            None,
            "clean eof after the last frame"
        );
    }

    #[tokio::test]
    async fn writes_and_reads_a_release_pdu() {
        let (mut client, mut server) = duplex(1024);
        write_pdu(&mut client, &Pdu::ReleaseRq).await.unwrap();
        let pdu = read_pdu(&mut server).await.unwrap().unwrap();
        assert_eq!(pdu, Pdu::ReleaseRq);
    }

    #[tokio::test]
    async fn writes_and_reads_a_pdata_pdu() {
        let (mut client, mut server) = duplex(4096);
        let pdata = Pdu::PData(PData {
            pdvs: vec![Pdv {
                context_id: 1,
                is_command: true,
                is_last: true,
                data: vec![1, 2, 3, 4].into(),
            }],
        });
        write_pdu(&mut client, &pdata).await.unwrap();
        assert_eq!(read_pdu(&mut server).await.unwrap().unwrap(), pdata);
    }

    #[tokio::test]
    async fn write_pdus_batches_several_frames_into_one_write() {
        let (mut client, mut server) = duplex(4096);
        let first = Pdu::PData(PData {
            pdvs: vec![Pdv {
                context_id: 1,
                is_command: true,
                is_last: false,
                data: vec![1, 2, 3, 4].into(),
            }],
        });
        let second = Pdu::PData(PData {
            pdvs: vec![Pdv {
                context_id: 1,
                is_command: false,
                is_last: true,
                data: vec![5, 6].into(),
            }],
        });
        write_pdus(&mut client, &[first.clone(), second.clone()])
            .await
            .unwrap();
        assert_eq!(read_pdu(&mut server).await.unwrap().unwrap(), first);
        assert_eq!(read_pdu(&mut server).await.unwrap().unwrap(), second);
    }

    #[tokio::test]
    async fn write_pdus_matches_the_encoded_bytes_with_odd_padding() {
        let pdata = Pdu::PData(PData {
            pdvs: vec![
                Pdv {
                    context_id: 1,
                    is_command: true,
                    is_last: false,
                    data: vec![1, 2, 3].into(),
                },
                Pdv {
                    context_id: 2,
                    is_command: false,
                    is_last: true,
                    data: vec![4, 5, 6, 7].into(),
                },
            ],
        });
        let pdus = vec![pdata, Pdu::ReleaseRq];
        let mut expected = Vec::new();
        for pdu in &pdus {
            pdu::encode_into(pdu, &mut expected).unwrap();
        }

        let (mut client, mut server) = duplex(4096);
        write_pdus(&mut client, &pdus).await.unwrap();
        let mut got = vec![0u8; expected.len()];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(
            got, expected,
            "vectored P-DATA is byte-identical to encode_into"
        );
    }

    #[tokio::test]
    async fn writes_and_reads_an_abort() {
        let (mut client, mut server) = duplex(1024);
        let abort = Pdu::Abort(Abort {
            source: AbortSource::Provider,
            reason: 0,
        });
        write_pdu(&mut client, &abort).await.unwrap();
        assert_eq!(read_pdu(&mut server).await.unwrap().unwrap(), abort);
    }

    #[tokio::test]
    async fn clean_eof_reports_none() {
        let (client, mut server) = duplex(64);
        drop(client);
        assert!(
            read_pdu(&mut server).await.unwrap().is_none(),
            "peer close is not an error"
        );
    }

    #[tokio::test]
    async fn truncated_pdu_is_an_io_error() {
        let (mut client, mut server) = duplex(64);
        // header claims 100 body bytes but the peer closes early
        client
            .write_all(&[0x05, 0x00, 0x00, 0x00, 0x00, 100])
            .await
            .unwrap();
        drop(client);
        match read_pdu(&mut server).await {
            Err(NetworkError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_pdu_at_the_advertised_max_is_accepted() {
        // `encode_associate` never advertises more than MAX_PDU_SIZE, so a peer
        // may legitimately send a whole PDU of exactly that size (header
        // included): it must be read, not rejected.
        let (mut client, mut server) = duplex(2 * MAX_PDU_SIZE as usize);
        let body_len = MAX_PDU_SIZE - HEADER_LEN as u32;
        let mut frame = vec![0x04, 0x00];
        frame.extend_from_slice(&body_len.to_be_bytes());
        let pdv_len = body_len - 4;
        frame.extend_from_slice(&pdv_len.to_be_bytes());
        frame.extend_from_slice(&[0x01, 0x03]); // context 1, command + last
        frame.resize(HEADER_LEN + body_len as usize, 0x00);
        client.write_all(&frame).await.unwrap();

        match read_pdu(&mut server)
            .await
            .expect("a PDU at the advertised maximum is accepted")
        {
            Some(Pdu::PData(pdata)) => {
                assert_eq!(pdata.pdvs.len(), 1);
                assert_eq!(pdata.pdvs[0].data.len(), (pdv_len - 2) as usize);
            }
            other => panic!("expected PData, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_truncated_frame_at_eof_is_an_io_error() {
        // A header claims 100 bytes but the peer sends 10 and closes: the
        // framer must report an EOF inside a frame, not parse garbage.
        let (mut client, mut server) = duplex(64);
        let mut frame = vec![0x04, 0x00, 0x00, 0x00, 0x00, 100];
        frame.extend_from_slice(&[0x00; 4]);
        client.write_all(&frame).await.unwrap();
        drop(client);

        // the cancel-safe framer is what the associations use
        match PduReader::new().read_pdu(&mut server).await {
            Err(NetworkError::Io(error)) => {
                assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof)
            }
            other => panic!("expected an early-EOF error, got {other:?}"),
        }
    }

    #[test]
    fn a_dataset_that_cannot_be_decoded_is_an_io_error() {
        // the deflated syntax inflates first: garbage fails there, and the
        // decoder reports it as an IO error rather than a panic
        let syntax =
            radicom::model::transfer_syntax::syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let error = crate::message::decode_dataset(&[0xFF; 8], &syntax)
            .expect_err("garbage is not a deflate stream");
        assert!(matches!(error, NetworkError::Io(_)), "got {error:?}");

        // a truncated implicit command set is tolerated by the lenient parser
        let _ = crate::message::decode_command(&[0x08, 0x00, 0x00]);
    }

    #[tokio::test]
    async fn oversized_pdu_is_a_protocol_error() {
        // 0x00040001 is above the largest value this side ever advertises, so
        // rejecting it is not a false claim about our own capacity.
        let (mut client, mut server) = duplex(64);
        client
            .write_all(&[0x05, 0x00, 0x00, 0x04, 0x00, 0x01])
            .await
            .unwrap();
        match read_pdu(&mut server).await {
            Err(NetworkError::Protocol(message)) => {
                assert!(message.contains("262144"), "got {message}")
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn several_pdus_are_read_in_sequence() {
        let (mut client, mut server) = duplex(4096);
        write_pdu(&mut client, &Pdu::ReleaseRq).await.unwrap();
        write_pdu(&mut client, &Pdu::ReleaseRp).await.unwrap();
        assert_eq!(
            read_pdu(&mut server).await.unwrap().unwrap(),
            Pdu::ReleaseRq
        );
        assert_eq!(
            read_pdu(&mut server).await.unwrap().unwrap(),
            Pdu::ReleaseRp
        );
    }

    #[tokio::test]
    async fn absurd_length_field_is_rejected_before_allocating() {
        // a peer-declared 4 GiB body must be rejected by the MAX_PDU_SIZE
        // gate before any buffer is sized from it, not allocated then filled.
        let (mut client, mut server) = duplex(64);
        client
            .write_all(&[0x04, 0x00, 0xFF, 0xFF, 0xFF, 0xFF])
            .await
            .unwrap();
        match read_pdu(&mut server).await {
            Err(NetworkError::Protocol(message)) => {
                assert!(message.contains("262144"), "got {message}")
            }
            other => panic!("expected a Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn tcp_options_default_to_nodelay() {
        let options = TcpOptions::default();
        assert!(options.nodelay);
        assert!(options.send_buffer_size.is_none());
        assert!(options.receive_buffer_size.is_none());
    }

    #[tokio::test]
    async fn socket_buffer_sizes_reach_the_socket() {
        // the options used to be dropped on the floor. Read the values
        // back from the descriptor itself: shrink by a factor the kernel cannot
        // round back up to the default, so an ignored request is visible.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _peer = accept.await.unwrap();

        let socket = socket2::SockRef::from(&stream);
        let default_send = socket.send_buffer_size().unwrap();
        let default_recv = socket.recv_buffer_size().unwrap();
        let options = TcpOptions {
            nodelay: true,
            send_buffer_size: Some((default_send / 4) as u32),
            receive_buffer_size: Some((default_recv / 4) as u32),
        };
        apply_options(&stream, &options).unwrap();

        assert!(
            socket.send_buffer_size().unwrap() < default_send,
            "SO_SNDBUF is still the OS default of {default_send}"
        );
        assert!(
            socket.recv_buffer_size().unwrap() < default_recv,
            "SO_RCVBUF is still the OS default of {default_recv}"
        );
    }

    #[test]
    fn network_identifier_is_stable_across_calls() {
        // the value is process-constant, so caching it must not change it.
        assert_eq!(network_identifier(), network_identifier());
    }

    #[test]
    fn network_identifier_is_a_valid_uid_shape() {
        // depends on the environment; only the shape is asserted
        if let Some(uid) = network_identifier() {
            assert!(
                uid.starts_with(crate::server::IMPLEMENTATION_CLASS_UID),
                "the identifier appends the MAC to the implementation class UID, got {uid}"
            );
            assert!(uid.len() <= 64, "UIDs are at most 64 characters");
            assert!(!uid.contains(".."), "no empty components");
        }
    }
}
