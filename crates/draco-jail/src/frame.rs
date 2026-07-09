//! IPC frame codec for the supervisor <-> jailed-child channel (canonical spec §6).
//!
//! One bidirectional `AF_UNIX` `SOCK_STREAM` socketpair carries length-prefixed
//! frames. Because the transport is a byte stream (not a datagram socket), every
//! frame is self-describing and the reader is written to tolerate short reads: a
//! single logical frame may arrive across many `read()` syscalls.
//!
//! ## Wire format
//!
//! ```text
//! offset  size  field
//! 0       2     magic       = b"DR"
//! 2       1     version     = 1
//! 3       1     msg_class   = 1 (SupervisorToJail) | 2 (JailToSupervisor)
//! 4       4     header_len  u32, little-endian
//! 8       4     body_len    u32, little-endian
//! 12      H     header      `header_len` bytes of UTF-8 JSON (the typed frame enum)
//! 12+H    B     body        `body_len` raw bytes (HTML / request body / empty)
//! ```
//!
//! The 12-byte fixed prefix lets a reader learn both variable lengths before it
//! commits to allocating; both lengths are validated against hard caps
//! ([`MAX_HEADER_LEN`], [`MAX_BODY_LEN`]) *before* any allocation so a hostile or
//! corrupt peer cannot drive an out-of-memory abort.
//!
//! The header is one of the frozen `draco-types` enums serialized as JSON; the
//! `msg_class` byte lets the reader pick the right type without trial
//! deserialization and rejects a frame sent in the wrong direction.

use std::io::{self, Read, Write};

use draco_types::{JailToSupervisor, SupervisorToJail};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Frame magic: the two ASCII bytes `DR`.
pub const MAGIC: [u8; 2] = *b"DR";

/// Wire-format version. Bumped on any incompatible framing change.
pub const VERSION: u8 = 1;

/// Size of the fixed frame prefix (magic + version + class + two u32 lengths).
pub const PREFIX_LEN: usize = 12;

/// Maximum accepted JSON header size (64 KiB). Headers are small typed control
/// messages; anything larger is treated as a protocol violation, not payload.
pub const MAX_HEADER_LEN: u32 = 64 * 1024;

/// Maximum accepted body size (32 MiB). The body carries raw HTML or a request
/// body; this cap bounds a single frame's allocation.
pub const MAX_BODY_LEN: u32 = 32 * 1024 * 1024;

/// Which direction a frame travels, encoded in the `msg_class` byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgClass {
    /// Header is a [`SupervisorToJail`] value.
    SupervisorToJail = 1,
    /// Header is a [`JailToSupervisor`] value.
    JailToSupervisor = 2,
}

impl MsgClass {
    fn from_byte(b: u8) -> Result<Self, FrameError> {
        match b {
            1 => Ok(MsgClass::SupervisorToJail),
            2 => Ok(MsgClass::JailToSupervisor),
            other => Err(FrameError::BadMsgClass(other)),
        }
    }
}

/// A frame that travels supervisor -> jailed child. The optional body carries
/// raw HTML (for `Hydrate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorFrame {
    /// The typed control header.
    pub header: SupervisorToJail,
    /// Out-of-band raw bytes (empty when unused).
    pub body: Vec<u8>,
}

/// A frame that travels jailed child -> supervisor. The optional body carries an
/// intercepted request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailFrame {
    /// The typed control header.
    pub header: JailToSupervisor,
    /// Out-of-band raw bytes (empty when unused).
    pub body: Vec<u8>,
}

/// Errors from encoding or decoding a frame.
#[derive(Debug)]
pub enum FrameError {
    /// The transport returned EOF cleanly on a frame boundary (no partial frame
    /// buffered). Callers treat this as an orderly channel close.
    Eof,
    /// Underlying transport I/O error (includes a *partial* frame being cut off
    /// mid-read, surfaced as [`io::ErrorKind::UnexpectedEof`]).
    Io(io::Error),
    /// Magic bytes were not `DR`.
    BadMagic([u8; 2]),
    /// Version byte did not match [`VERSION`].
    BadVersion(u8),
    /// `msg_class` byte was neither 1 nor 2.
    BadMsgClass(u8),
    /// A frame in the wrong direction for the expected type was received.
    WrongDirection {
        /// The class the caller expected to decode.
        expected: MsgClass,
        /// The class actually present on the wire.
        got: MsgClass,
    },
    /// `header_len` exceeded [`MAX_HEADER_LEN`].
    HeaderTooLarge(u32),
    /// `body_len` exceeded [`MAX_BODY_LEN`].
    BodyTooLarge(u32),
    /// The JSON header failed to (de)serialize.
    Json(serde_json::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Eof => write!(f, "channel closed at frame boundary (EOF)"),
            FrameError::Io(e) => write!(f, "frame I/O error: {e}"),
            FrameError::BadMagic(m) => write!(f, "bad frame magic: {m:?} (expected b\"DR\")"),
            FrameError::BadVersion(v) => {
                write!(f, "unsupported frame version: {v} (expected {VERSION})")
            }
            FrameError::BadMsgClass(c) => write!(f, "invalid msg_class byte: {c}"),
            FrameError::WrongDirection { expected, got } => {
                write!(
                    f,
                    "frame direction mismatch: expected {expected:?}, got {got:?}"
                )
            }
            FrameError::HeaderTooLarge(n) => {
                write!(f, "header_len {n} exceeds cap {MAX_HEADER_LEN}")
            }
            FrameError::BodyTooLarge(n) => write!(f, "body_len {n} exceeds cap {MAX_BODY_LEN}"),
            FrameError::Json(e) => write!(f, "frame header JSON error: {e}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameError::Io(e) => Some(e),
            FrameError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for FrameError {
    fn from(e: serde_json::Error) -> Self {
        FrameError::Json(e)
    }
}

/// Serialize a typed header + raw body into a single framed byte buffer.
///
/// Returns [`FrameError::HeaderTooLarge`] / [`FrameError::BodyTooLarge`] if the
/// encoded sizes would exceed the wire caps, so an over-large frame is rejected
/// at the sender rather than blowing up the receiver.
fn encode<H: Serialize>(class: MsgClass, header: &H, body: &[u8]) -> Result<Vec<u8>, FrameError> {
    let header_json = serde_json::to_vec(header)?;
    if header_json.len() > MAX_HEADER_LEN as usize {
        return Err(FrameError::HeaderTooLarge(header_json.len() as u32));
    }
    if body.len() > MAX_BODY_LEN as usize {
        return Err(FrameError::BodyTooLarge(body.len() as u32));
    }

    let mut out = Vec::with_capacity(PREFIX_LEN + header_json.len() + body.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(class as u8);
    out.extend_from_slice(&(header_json.len() as u32).to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&header_json);
    out.extend_from_slice(body);
    Ok(out)
}

/// Write a supervisor -> child frame to `w`.
pub fn write_supervisor_frame<W: Write>(
    w: &mut W,
    header: &SupervisorToJail,
    body: &[u8],
) -> Result<(), FrameError> {
    let buf = encode(MsgClass::SupervisorToJail, header, body)?;
    w.write_all(&buf).map_err(FrameError::Io)?;
    w.flush().map_err(FrameError::Io)
}

/// Write a child -> supervisor frame to `w`.
pub fn write_jail_frame<W: Write>(
    w: &mut W,
    header: &JailToSupervisor,
    body: &[u8],
) -> Result<(), FrameError> {
    let buf = encode(MsgClass::JailToSupervisor, header, body)?;
    w.write_all(&buf).map_err(FrameError::Io)?;
    w.flush().map_err(FrameError::Io)
}

/// Read exactly `buf.len()` bytes, distinguishing a clean frame-boundary EOF
/// (nothing read at all) from a truncated frame (some bytes read, then EOF).
///
/// `Read::read_exact` already loops over short reads; we only special-case the
/// boundary so the caller can tell an orderly close from a corrupt stream.
fn read_exact_boundary<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), FrameError> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            // `read_exact` reports UnexpectedEof both for "0 bytes at the start"
            // and "cut off partway". Treat the prefix's first read as the frame
            // boundary via the dedicated prefix reader below; here it is always
            // a truncated read.
            Err(FrameError::Io(e))
        }
        Err(e) => Err(FrameError::Io(e)),
    }
}

/// Decoded fixed prefix.
struct Prefix {
    class: MsgClass,
    header_len: u32,
    body_len: u32,
}

/// Read and validate the 12-byte prefix. Returns [`FrameError::Eof`] iff the
/// stream is at a clean frame boundary (the very first byte read hits EOF).
fn read_prefix<R: Read>(r: &mut R) -> Result<Prefix, FrameError> {
    let mut prefix = [0u8; PREFIX_LEN];

    // Read the first byte on its own so a closed channel is reported as a clean
    // `Eof` rather than a truncated-frame I/O error. Retry on EINTR so a signal
    // does not corrupt the frame or spuriously report EOF.
    loop {
        match r.read(&mut prefix[..1]) {
            Ok(0) => return Err(FrameError::Eof),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    // Fill the remaining 11 bytes; from here on EOF means a truncated frame.
    read_exact_boundary(r, &mut prefix[1..])?;

    if prefix[0..2] != MAGIC {
        return Err(FrameError::BadMagic([prefix[0], prefix[1]]));
    }
    if prefix[2] != VERSION {
        return Err(FrameError::BadVersion(prefix[2]));
    }
    let class = MsgClass::from_byte(prefix[3])?;
    let header_len = u32::from_le_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]);
    let body_len = u32::from_le_bytes([prefix[8], prefix[9], prefix[10], prefix[11]]);

    if header_len > MAX_HEADER_LEN {
        return Err(FrameError::HeaderTooLarge(header_len));
    }
    if body_len > MAX_BODY_LEN {
        return Err(FrameError::BodyTooLarge(body_len));
    }
    Ok(Prefix {
        class,
        header_len,
        body_len,
    })
}

/// Read a frame of `expected` class, returning its JSON-decoded header plus body.
fn read_frame<R: Read, H: DeserializeOwned>(
    r: &mut R,
    expected: MsgClass,
) -> Result<(H, Vec<u8>), FrameError> {
    let prefix = read_prefix(r)?;
    if prefix.class != expected {
        // Drain the announced payload so the stream stays frame-aligned even
        // though we reject this one, then surface the direction error.
        let to_skip = prefix.header_len as usize + prefix.body_len as usize;
        let mut scratch = vec![0u8; to_skip];
        // Best-effort drain; ignore its error in favor of the direction error.
        let _ = read_exact_boundary(r, &mut scratch);
        return Err(FrameError::WrongDirection {
            expected,
            got: prefix.class,
        });
    }

    // Caps were validated in `read_prefix`, so these allocations are bounded.
    let mut header_buf = vec![0u8; prefix.header_len as usize];
    read_exact_boundary(r, &mut header_buf)?;
    let mut body = vec![0u8; prefix.body_len as usize];
    read_exact_boundary(r, &mut body)?;

    let header: H = serde_json::from_slice(&header_buf)?;
    Ok((header, body))
}

/// Read one supervisor -> child frame from `r`.
///
/// Returns [`FrameError::Eof`] if the channel closed cleanly on a frame boundary.
pub fn read_supervisor_frame<R: Read>(r: &mut R) -> Result<SupervisorFrame, FrameError> {
    let (header, body) = read_frame::<R, SupervisorToJail>(r, MsgClass::SupervisorToJail)?;
    Ok(SupervisorFrame { header, body })
}

/// Read one child -> supervisor frame from `r`.
///
/// Returns [`FrameError::Eof`] if the channel closed cleanly on a frame boundary.
pub fn read_jail_frame<R: Read>(r: &mut R) -> Result<JailFrame, FrameError> {
    let (header, body) = read_frame::<R, JailToSupervisor>(r, MsgClass::JailToSupervisor)?;
    Ok(JailFrame { header, body })
}

// ===========================================================================
// Async frame I/O (Option B: tokio::net::UnixStream transport).
//
// Byte-identical wire format to the sync helpers above — same `encode`, same
// 12-byte prefix + JSON header + body, same caps and clean-EOF semantics. These
// drive the fully-async, concurrent IPC: the jailed child runs a tokio reactor
// (the strict seccomp allowlist already permits epoll/eventfd2) so a page's
// `import()` chunk loads fan out over the socket without freezing the V8 thread,
// and the supervisor serves fetches + writes replies out-of-order on its
// multi-thread runtime.
// ===========================================================================

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Write a supervisor -> child frame to an async writer.
pub async fn write_supervisor_frame_async<W: AsyncWrite + Unpin>(
    w: &mut W,
    header: &SupervisorToJail,
    body: &[u8],
) -> Result<(), FrameError> {
    let buf = encode(MsgClass::SupervisorToJail, header, body)?;
    w.write_all(&buf).await.map_err(FrameError::Io)?;
    w.flush().await.map_err(FrameError::Io)
}

/// Write a child -> supervisor frame to an async writer.
pub async fn write_jail_frame_async<W: AsyncWrite + Unpin>(
    w: &mut W,
    header: &JailToSupervisor,
    body: &[u8],
) -> Result<(), FrameError> {
    let buf = encode(MsgClass::JailToSupervisor, header, body)?;
    w.write_all(&buf).await.map_err(FrameError::Io)?;
    w.flush().await.map_err(FrameError::Io)
}

/// Async twin of [`read_prefix`]: returns [`FrameError::Eof`] iff the very first
/// byte hits EOF (a clean frame-boundary close); EOF thereafter is a truncated
/// frame surfaced as [`FrameError::Io`].
async fn read_prefix_async<R: AsyncRead + Unpin>(r: &mut R) -> Result<Prefix, FrameError> {
    let mut prefix = [0u8; PREFIX_LEN];
    match r.read(&mut prefix[..1]).await {
        Ok(0) => return Err(FrameError::Eof),
        Ok(_) => {}
        Err(e) => return Err(FrameError::Io(e)),
    }
    r.read_exact(&mut prefix[1..]).await.map_err(FrameError::Io)?;

    if prefix[0..2] != MAGIC {
        return Err(FrameError::BadMagic([prefix[0], prefix[1]]));
    }
    if prefix[2] != VERSION {
        return Err(FrameError::BadVersion(prefix[2]));
    }
    let class = MsgClass::from_byte(prefix[3])?;
    let header_len = u32::from_le_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]);
    let body_len = u32::from_le_bytes([prefix[8], prefix[9], prefix[10], prefix[11]]);
    if header_len > MAX_HEADER_LEN {
        return Err(FrameError::HeaderTooLarge(header_len));
    }
    if body_len > MAX_BODY_LEN {
        return Err(FrameError::BodyTooLarge(body_len));
    }
    Ok(Prefix {
        class,
        header_len,
        body_len,
    })
}

async fn read_frame_async<R: AsyncRead + Unpin, H: DeserializeOwned>(
    r: &mut R,
    expected: MsgClass,
) -> Result<(H, Vec<u8>), FrameError> {
    let prefix = read_prefix_async(r).await?;
    if prefix.class != expected {
        // Drain the announced payload so the stream stays frame-aligned.
        let to_skip = prefix.header_len as usize + prefix.body_len as usize;
        let mut scratch = vec![0u8; to_skip];
        let _ = r.read_exact(&mut scratch).await;
        return Err(FrameError::WrongDirection {
            expected,
            got: prefix.class,
        });
    }
    let mut header_buf = vec![0u8; prefix.header_len as usize];
    r.read_exact(&mut header_buf).await.map_err(FrameError::Io)?;
    let mut body = vec![0u8; prefix.body_len as usize];
    r.read_exact(&mut body).await.map_err(FrameError::Io)?;
    let header: H = serde_json::from_slice(&header_buf)?;
    Ok((header, body))
}

/// Read one supervisor -> child frame from an async reader.
pub async fn read_supervisor_frame_async<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<SupervisorFrame, FrameError> {
    let (header, body) =
        read_frame_async::<R, SupervisorToJail>(r, MsgClass::SupervisorToJail).await?;
    Ok(SupervisorFrame { header, body })
}

/// Read one child -> supervisor frame from an async reader.
pub async fn read_jail_frame_async<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<JailFrame, FrameError> {
    let (header, body) =
        read_frame_async::<R, JailToSupervisor>(r, MsgClass::JailToSupervisor).await?;
    Ok(JailFrame { header, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use draco_types::{JailKind, LogLevel, RuntimeOutcome};
    use std::io::Cursor;

    fn hydrate() -> SupervisorToJail {
        SupervisorToJail::Hydrate {
            url: "https://example.com/p/1".into(),
            capture_window_ms: 2000,
            quiesce_ms: 300,
            max_intercepts: 64,
            stub_response_json: "{}".into(),
        }
    }

    #[test]
    fn supervisor_frame_roundtrips_with_body() {
        let body = b"<html><body>hi</body></html>".to_vec();
        let mut buf = Vec::new();
        write_supervisor_frame(&mut buf, &hydrate(), &body).unwrap();

        // Prefix sanity: magic, version, class.
        assert_eq!(&buf[0..2], b"DR");
        assert_eq!(buf[2], VERSION);
        assert_eq!(buf[3], MsgClass::SupervisorToJail as u8);

        let mut cur = Cursor::new(buf);
        let got = read_supervisor_frame(&mut cur).unwrap();
        assert_eq!(got.header, hydrate());
        assert_eq!(got.body, body);
    }

    #[test]
    fn jail_frame_roundtrips_empty_body() {
        let hdr = JailToSupervisor::Ready {
            snapshot_restore_ms: 7,
        };
        let mut buf = Vec::new();
        write_jail_frame(&mut buf, &hdr, &[]).unwrap();
        assert_eq!(buf[3], MsgClass::JailToSupervisor as u8);

        let mut cur = Cursor::new(buf);
        let got = read_jail_frame(&mut cur).unwrap();
        assert_eq!(got.header, hdr);
        assert!(got.body.is_empty());
    }

    #[test]
    fn multiple_frames_stream_in_order() {
        let mut buf = Vec::new();
        write_supervisor_frame(&mut buf, &hydrate(), b"first").unwrap();
        write_supervisor_frame(&mut buf, &SupervisorToJail::Shutdown, b"").unwrap();

        let mut cur = Cursor::new(buf);
        let f1 = read_supervisor_frame(&mut cur).unwrap();
        assert_eq!(f1.header, hydrate());
        assert_eq!(f1.body, b"first");
        let f2 = read_supervisor_frame(&mut cur).unwrap();
        assert_eq!(f2.header, SupervisorToJail::Shutdown);
        assert!(f2.body.is_empty());
    }

    #[test]
    fn clean_eof_on_boundary_reports_eof() {
        let mut cur = Cursor::new(Vec::<u8>::new());
        match read_supervisor_frame(&mut cur) {
            Err(FrameError::Eof) => {}
            other => panic!("expected Eof, got {other:?}"),
        }
    }

    #[test]
    fn truncated_prefix_is_io_error_not_eof() {
        // Only 5 of the 12 prefix bytes are present.
        let mut buf = Vec::new();
        write_supervisor_frame(&mut buf, &hydrate(), b"body").unwrap();
        buf.truncate(5);
        let mut cur = Cursor::new(buf);
        match read_supervisor_frame(&mut cur) {
            Err(FrameError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }

    #[test]
    fn truncated_body_is_io_error() {
        let mut buf = Vec::new();
        write_supervisor_frame(&mut buf, &hydrate(), b"0123456789").unwrap();
        // Drop the last few body bytes.
        buf.truncate(buf.len() - 4);
        let mut cur = Cursor::new(buf);
        match read_supervisor_frame(&mut cur) {
            Err(FrameError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }

    #[test]
    fn short_reads_are_reassembled() {
        // A reader that yields at most 1 byte per call exercises the short-read
        // reassembly path in `read_exact`.
        struct DripReader {
            data: Vec<u8>,
            pos: usize,
        }
        impl Read for DripReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.pos >= self.data.len() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.data[self.pos];
                self.pos += 1;
                Ok(1)
            }
        }

        let mut buf = Vec::new();
        write_supervisor_frame(&mut buf, &hydrate(), b"dripped body bytes").unwrap();
        let mut drip = DripReader { data: buf, pos: 0 };
        let got = read_supervisor_frame(&mut drip).unwrap();
        assert_eq!(got.header, hydrate());
        assert_eq!(got.body, b"dripped body bytes");
    }

    #[test]
    fn eintr_on_first_byte_is_retried_not_corrupted() {
        // A reader that returns EINTR on its very first call, then serves the
        // real bytes. The prefix reader must retry byte 0 rather than skip it
        // (which would misalign the whole frame) or report a spurious EOF.
        struct EintrOnceReader {
            data: Vec<u8>,
            pos: usize,
            interrupted: bool,
        }
        impl Read for EintrOnceReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                if self.pos >= self.data.len() || buf.is_empty() {
                    return Ok(0);
                }
                let n = buf.len().min(self.data.len() - self.pos);
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                Ok(n)
            }
        }

        let mut buf = Vec::new();
        write_supervisor_frame(&mut buf, &hydrate(), b"payload").unwrap();
        let mut reader = EintrOnceReader {
            data: buf,
            pos: 0,
            interrupted: false,
        };
        let got = read_supervisor_frame(&mut reader).unwrap();
        assert_eq!(got.header, hydrate());
        assert_eq!(got.body, b"payload");
    }

    #[test]
    fn bad_magic_rejected() {
        let mut buf = Vec::new();
        write_jail_frame(
            &mut buf,
            &JailToSupervisor::Log {
                level: LogLevel::Info,
                msg: "x".into(),
            },
            &[],
        )
        .unwrap();
        buf[0] = b'X';
        let mut cur = Cursor::new(buf);
        match read_jail_frame(&mut cur) {
            Err(FrameError::BadMagic(m)) => assert_eq!(m, [b'X', b'R']),
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn bad_version_rejected() {
        let mut buf = Vec::new();
        write_supervisor_frame(&mut buf, &SupervisorToJail::Shutdown, &[]).unwrap();
        buf[2] = 99;
        let mut cur = Cursor::new(buf);
        match read_supervisor_frame(&mut cur) {
            Err(FrameError::BadVersion(99)) => {}
            other => panic!("expected BadVersion(99), got {other:?}"),
        }
    }

    #[test]
    fn wrong_direction_rejected() {
        // Encode a jail frame, then try to read it as a supervisor frame.
        let mut buf = Vec::new();
        write_jail_frame(
            &mut buf,
            &JailToSupervisor::Result {
                outcome: RuntimeOutcome::Quiesced,
                intercept_count: 3,
            },
            b"trailing",
        )
        .unwrap();
        let mut cur = Cursor::new(buf);
        match read_supervisor_frame(&mut cur) {
            Err(FrameError::WrongDirection { expected, got }) => {
                assert_eq!(expected, MsgClass::SupervisorToJail);
                assert_eq!(got, MsgClass::JailToSupervisor);
            }
            other => panic!("expected WrongDirection, got {other:?}"),
        }
    }

    #[test]
    fn oversized_header_len_rejected_by_reader() {
        // Hand-craft a prefix that claims a header larger than MAX_HEADER_LEN.
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        buf.push(VERSION);
        buf.push(MsgClass::SupervisorToJail as u8);
        buf.extend_from_slice(&(MAX_HEADER_LEN + 1).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        let mut cur = Cursor::new(buf);
        match read_supervisor_frame(&mut cur) {
            Err(FrameError::HeaderTooLarge(n)) => assert_eq!(n, MAX_HEADER_LEN + 1),
            other => panic!("expected HeaderTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn oversized_body_len_rejected_by_reader() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        buf.push(VERSION);
        buf.push(MsgClass::JailToSupervisor as u8);
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&(MAX_BODY_LEN + 1).to_le_bytes());
        let mut cur = Cursor::new(buf);
        match read_jail_frame(&mut cur) {
            Err(FrameError::BodyTooLarge(n)) => assert_eq!(n, MAX_BODY_LEN + 1),
            other => panic!("expected BodyTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn oversized_body_rejected_by_writer() {
        // The writer must refuse to encode a body over the cap without allocating
        // the whole frame; we simulate by asking to encode a huge (fake) length.
        // Use a real but oversized buffer guardedly: allocate cap+1 zero bytes.
        let body = vec![0u8; MAX_BODY_LEN as usize + 1];
        let mut sink = Vec::new();
        match write_supervisor_frame(&mut sink, &SupervisorToJail::Shutdown, &body) {
            Err(FrameError::BodyTooLarge(n)) => assert_eq!(n, MAX_BODY_LEN + 1),
            other => panic!("expected BodyTooLarge, got {other:?}"),
        }
        assert!(sink.is_empty(), "nothing should be written on rejection");
    }

    #[test]
    fn bad_msg_class_rejected() {
        let mut buf = Vec::new();
        write_supervisor_frame(&mut buf, &SupervisorToJail::Shutdown, &[]).unwrap();
        buf[3] = 7; // neither 1 nor 2
        let mut cur = Cursor::new(buf);
        match read_supervisor_frame(&mut cur) {
            Err(FrameError::BadMsgClass(7)) => {}
            other => panic!("expected BadMsgClass(7), got {other:?}"),
        }
    }

    #[test]
    fn error_frame_with_jail_kind_roundtrips() {
        let hdr = JailToSupervisor::Error {
            reason: JailKind::Killed,
            detail: "seccomp SIGSYS".into(),
        };
        let mut buf = Vec::new();
        write_jail_frame(&mut buf, &hdr, &[]).unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(read_jail_frame(&mut cur).unwrap().header, hdr);
    }
}
