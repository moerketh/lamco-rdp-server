//! Handshake-deadline stream wrapper — mitigation for the dead-client wedge.
//!
//! Extracted to the `hyperv-rdp-extras` crate (MIT; see that repo's
//! `PROVENANCE.md` for the audit trail). This module preserves the fork's
//! import paths via re-exports; the wrapper is generic over any
//! `AsyncRead + AsyncWrite + Unpin` tokio stream, and the blanket
//! `AsyncRdpStream` impl in `listener` covers it exactly as before.
//!
//! [`HandshakeDeadlineStream`] enforces a one-shot deadline on the *first*
//! client read of a freshly accepted connection:
//!
//! - If the peer sends nothing before the deadline elapses, the read errors
//!   with [`io::ErrorKind::TimedOut`], the acceptor aborts that connection,
//!   and the dispatcher moves on to the next client.
//! - Once any bytes have arrived (or any write happened), the deadline is
//!   cleared forever — idle-but-established sessions are untouched.

pub use hyperv_rdp_extras::transport::{
    DEFAULT_HANDSHAKE_DEADLINE, HandshakeDeadlineStream, log_deadline_rejection,
};
