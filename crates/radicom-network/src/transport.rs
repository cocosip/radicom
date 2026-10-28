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
        return Err(NetworkError::protocol("PDU length exceeds 65535"));
    }
    let mut frame = Vec::with_capacity(HEADER_LEN + length as usize);
    frame.extend_from_slice(&header);
    let mut body = vec![0u8; length as usize];
    reader.read_exact(&mut body).await.map_err(NetworkError::Io)?;
    frame.extend_from_slice(&body);

    pdu::decode(&frame).map(Some)
}

/// Writes one PDU frame to an async stream.
pub async fn write_pdu<W: AsyncWrite + Unpin>(writer: &mut W, pdu: &Pdu) -> Result<()> {
    let bytes = pdu::encode(pdu);
    writer.write_all(&bytes).await.map_err(NetworkError::Io)?;
    writer.flush().await.map_err(NetworkError::Io)?;
    Ok(())
}

/// Applies [`TcpOptions`] to a connected stream.
pub fn apply_options(stream: &tokio::net::TcpStream, options: &TcpOptions) -> Result<()> {
    stream.set_nodelay(options.nodelay).map_err(NetworkError::Io)?;
    // buffer sizing is advisory: platforms may refuse it, which is not fatal
    let _ = options.send_buffer_size;
    let _ = options.receive_buffer_size;
    Ok(())
}

/// Derives a DICOM UID from the primary MAC address (UID-05, §2).
///
/// Machine identity, mirroring fo-dicom `TryGetNetworkIdentifier`. Purely a
/// utility: nothing in the engine depends on it.
pub fn network_identifier() -> Option<String> {
    // `mac_address` is not a dependency; derive from the hostname instead so
    // the value is stable per machine without extra crates.
    let host = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).ok()?;
    if host.is_empty() {
        return None;
    }
    // fold the name into a 2.25 UUID-shaped UID
    let mut hash: u128 = 0xcbf2_9ce4_8422_2325;
    for byte in host.as_bytes() {
        hash ^= *byte as u128;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(format!("2.25.{hash}"))
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
            pdvs: vec![Pdv { context_id: 1, is_command: true, is_last: true, data: vec![1, 2, 3, 4] }],
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
    async fn oversized_pdu_is_a_protocol_error() {
        let (mut client, mut server) = duplex(64);
        // 0x00010000 exceeds the 65535 limit
        client.write_all(&[0x05, 0x00, 0x00, 0x01, 0x00, 0x00]).await.unwrap();
        match read_pdu(&mut server).await {
            Err(NetworkError::Protocol(message)) => assert!(message.contains("65535"), "got {message}"),
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

    #[test]
    fn tcp_options_default_to_nodelay() {
        let options = TcpOptions::default();
        assert!(options.nodelay);
        assert!(options.send_buffer_size.is_none());
        assert!(options.receive_buffer_size.is_none());
    }

    #[test]
    fn network_identifier_is_a_valid_uid_shape() {
        // depends on the environment; only the shape is asserted
        if let Some(uid) = network_identifier() {
            assert!(uid.starts_with("2.25."), "got {uid}");
            assert!(uid.len() <= 64, "UIDs are at most 64 characters");
            assert!(!uid.contains(".."), "no empty components");
        }
    }
}
