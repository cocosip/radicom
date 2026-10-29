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

/// Options applied to a TCP connection (§2).
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
        TcpOptions { nodelay: true, send_buffer_size: None, receive_buffer_size: None }
    }
}

/// Reads one PDU frame from an async stream (§3.1).
///
/// Returns `Ok(None)` on a clean end of stream before any header byte, which
/// the engine treats as a peer close (§5.1).
pub async fn read_pdu<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Pdu>> {
    let mut header = [0u8; HEADER_LEN];
    // a clean EOF before the first byte is a normal close
    match reader.read_exact(&mut header[..1]).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(NetworkError::Io(e)),
    }
    reader.read_exact(&mut header[1..]).await.map_err(NetworkError::Io)?;

    let length = u32::from_be_bytes([header[2], header[3], header[4], header[5]]);
    if length > MAX_PDU_SIZE {
        return Err(NetworkError::protocol("PDU length exceeds the 262144-byte receive limit"));
    }
    // One allocation sized from the validated length field, then read the body
    // straight into it: no intermediate zero-initialised copy (§3.1). The
    // MAX_PDU_SIZE check above keeps a peer from requesting a huge allocation.
    let mut frame = vec![0u8; HEADER_LEN + length as usize];
    frame[..HEADER_LEN].copy_from_slice(&header);
    reader.read_exact(&mut frame[HEADER_LEN..]).await.map_err(NetworkError::Io)?;

    // Hand the frame over as `Bytes` so PDVs share it instead of copying (§3.4).
    pdu::decode_bytes(bytes::Bytes::from(frame)).map(Some)
}

/// Writes one PDU frame to an async stream.
///
/// Ceiling: `pdu` is a structured value, so it has to be encoded on every call;
/// the signature gives no place to hand in a reusable buffer, and the call sites
/// all hold a `&Pdu` rather than bytes. Removing that per-send allocation would
/// require threading a caller-owned encode buffer through the engine (§3.2).
pub async fn write_pdu<W: AsyncWrite + Unpin>(writer: &mut W, pdu: &Pdu) -> Result<()> {
    let bytes = pdu::encode(pdu)?;
    writer.write_all(&bytes).await.map_err(NetworkError::Io)?;
    writer.flush().await.map_err(NetworkError::Io)?;
    Ok(())
}

/// Applies [`TcpOptions`] to a connected stream.
///
/// Socket buffer sizing is advisory — the kernel may round the value up or cap
/// it — so a refused request is not fatal. It is at least *attempted*: the
/// fields would otherwise be a promise the transport never keeps (§2.12).
pub fn apply_options(stream: &tokio::net::TcpStream, options: &TcpOptions) -> Result<()> {
    stream.set_nodelay(options.nodelay).map_err(NetworkError::Io)?;
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

/// Derives a DICOM UID from the primary MAC address (UID-05, §2).
///
/// Machine identity, mirroring fo-dicom `TryGetNetworkIdentifier`. Purely a
/// utility: nothing in the engine depends on it.
pub fn network_identifier() -> Option<String> {
    // Read the environment / MAC once: the value is constant for the process,
    // so callers do not pay the lookup on every call (§3.7).
    static IDENTIFIER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    IDENTIFIER
        .get_or_init(|| {
            // One identity source for the whole crate: append the host MAC to
            // the implementation class UID, as fo-dicom's
            // NetworkManager.TryGetNetworkIdentifier does. (The old hostname
            // hash here disagreed with print_sop's MAC form, §1/§3.7.)
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
            pdvs: vec![Pdv { context_id: 1, is_command: true, is_last: true, data: vec![1, 2, 3, 4].into() }],
        });
        write_pdu(&mut client, &pdata).await.unwrap();
        assert_eq!(read_pdu(&mut server).await.unwrap().unwrap(), pdata);
    }

    #[tokio::test]
    async fn writes_and_reads_an_abort() {
        let (mut client, mut server) = duplex(1024);
        let abort = Pdu::Abort(Abort { source: AbortSource::Provider, reason: 0 });
        write_pdu(&mut client, &abort).await.unwrap();
        assert_eq!(read_pdu(&mut server).await.unwrap().unwrap(), abort);
    }

    #[tokio::test]
    async fn clean_eof_reports_none() {
        let (client, mut server) = duplex(64);
        drop(client);
        assert!(read_pdu(&mut server).await.unwrap().is_none(), "peer close is not an error");
    }

    #[tokio::test]
    async fn truncated_pdu_is_an_io_error() {
        let (mut client, mut server) = duplex(64);
        // header claims 100 body bytes but the peer closes early
        client.write_all(&[0x05, 0x00, 0x00, 0x00, 0x00, 100]).await.unwrap();
        drop(client);
        match read_pdu(&mut server).await {
            Err(NetworkError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_pdu_at_the_advertised_max_is_accepted() {
        // `encode_associate` never advertises more than MAX_PDU_SIZE, so a peer
        // may legitimately send a P-DATA-TF of exactly that size: it must be
        // read, not rejected.
        let (mut client, mut server) = duplex(2 * MAX_PDU_SIZE as usize);
        let mut frame = vec![0x04, 0x00];
        frame.extend_from_slice(&MAX_PDU_SIZE.to_be_bytes());
        let pdv_len = MAX_PDU_SIZE - 4;
        frame.extend_from_slice(&pdv_len.to_be_bytes());
        frame.extend_from_slice(&[0x01, 0x03]); // context 1, command + last
        frame.resize(HEADER_LEN + MAX_PDU_SIZE as usize, 0x00);
        client.write_all(&frame).await.unwrap();

        match read_pdu(&mut server).await.expect("a PDU at the advertised maximum is accepted") {
            Some(Pdu::PData(pdata)) => {
                assert_eq!(pdata.pdvs.len(), 1);
                assert_eq!(pdata.pdvs[0].data.len(), (pdv_len - 2) as usize);
            }
            other => panic!("expected PData, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_pdu_is_a_protocol_error() {
        // 0x00040001 is above the largest value this side ever advertises, so
        // rejecting it is not a false claim about our own capacity.
        let (mut client, mut server) = duplex(64);
        client.write_all(&[0x05, 0x00, 0x00, 0x04, 0x00, 0x01]).await.unwrap();
        match read_pdu(&mut server).await {
            Err(NetworkError::Protocol(message)) => assert!(message.contains("262144"), "got {message}"),
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn several_pdus_are_read_in_sequence() {
        let (mut client, mut server) = duplex(4096);
        write_pdu(&mut client, &Pdu::ReleaseRq).await.unwrap();
        write_pdu(&mut client, &Pdu::ReleaseRp).await.unwrap();
        assert_eq!(read_pdu(&mut server).await.unwrap().unwrap(), Pdu::ReleaseRq);
        assert_eq!(read_pdu(&mut server).await.unwrap().unwrap(), Pdu::ReleaseRp);
    }

    #[tokio::test]
    async fn absurd_length_field_is_rejected_before_allocating() {
        // §3.5: a peer-declared 4 GiB body must be rejected by the MAX_PDU_SIZE
        // gate before any buffer is sized from it, not allocated then filled.
        let (mut client, mut server) = duplex(64);
        client.write_all(&[0x04, 0x00, 0xFF, 0xFF, 0xFF, 0xFF]).await.unwrap();
        match read_pdu(&mut server).await {
            Err(NetworkError::Protocol(message)) => assert!(message.contains("262144"), "got {message}"),
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
        // §2.12: the options used to be dropped on the floor. Read the values
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
        // §3.7: the value is process-constant, so caching it must not change it.
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
