//! Handshake-deadline stream wrapper — mitigation for the dead-client wedge.
//!
//! 2026-08-27 capture finding (see `memories/repo/enhanced-session-capture-findings.md`):
//! a connection that completes TCP/vsock accept but never sends a byte parks the
//! IronRDP acceptor's first read *inside the serial accept dispatcher*, blacking
//! out every listener for as long as the silent peer keeps the socket open
//! (measured: 13.5 minutes; two queued vmconnect connections waited the entire
//! time in the listen backlog and only proceeded after the silent client died).
//!
//! [`HandshakeDeadlineStream`] enforces a one-shot deadline on the *first*
//! client read of a freshly accepted connection:
//!
//! - If the peer sends nothing before the deadline elapses, the read errors
//!   with [` io::ErrorKind::TimedOut`], the acceptor aborts that connection,
//!   and the dispatcher moves on to the next client.
//! - Once any bytes have arrived (or any write happened), the deadline is
//!   cleared forever — idle-but-established sessions are untouched. The
//!   timeout only guards the "connected, said nothing, holding the slot"
//!   state that stalls the serial accept loop.
//!
//! The wrapper is transport-agnostic and is installed by the accept
//! dispatcher for all acceptor modes.
//!
//! No pin projection is needed: `S: AsyncRdpStream` already implies
//! `S: Unpin`, and `tokio::time::Sleep` is `Unpin`, so the wrapper is
//! unconditionally `Unpin` and `Pin::get_mut` is sound in the poll impls.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::debug;

use super::listener::AsyncRdpStream;

/// Default window for the first client bytes after accept.
///
/// Generous by protocol standards: mstsc's pre-connect probe, vmms's relayed
/// X.224 Connection Request, and FreeRDP all send their first bytes within
/// tens of milliseconds of connect. Slow-path TLS handshakes still complete
/// the ClientHello well inside this window; the deadline covers
/// time-to-first-byte, not the whole handshake.
pub const DEFAULT_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(30);

/// Streams a connection that must produce its first byte within a deadline.
///
/// See the [module documentation](self) for the wedge this prevents.
pub struct HandshakeDeadlineStream<S> {
    inner: S,
    /// Boxed so the wrapper stays `Unpin` — `Sleep` itself contains a
    /// `PhantomPinned` and would otherwise make every wrapper `!Unpin`,
    /// which the blanket `AsyncRdpStream` impl (and thus the whole
    /// transport layer) requires.
    deadline: std::pin::Pin<Box<tokio::time::Sleep>>,
    armed: bool,
}

impl<S> HandshakeDeadlineStream<S>
where
    S: AsyncRdpStream,
{
    /// Wrap `stream` so its first read must complete within `deadline`.
    pub fn new(stream: S, deadline: Duration) -> Self {
        Self {
            inner: stream,
            deadline: Box::pin(tokio::time::sleep(deadline)),
            armed: true,
        }
    }
}

impl<S> AsyncRead for HandshakeDeadlineStream<S>
where
    S: AsyncRdpStream,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        if !this.armed {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }

        // Race the inner read against the deadline while armed. Once the
        // inner read is Ready the sleep is disarmed permanently and the
        // wrapper becomes a transparent passthrough.
        if let Poll::Ready(Ok(())) = Pin::new(&mut this.inner).poll_read(cx, buf) {
            this.armed = false;
            return Poll::Ready(Ok(()));
        }
        match this.deadline.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "handshake deadline elapsed before any client bytes arrived",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> AsyncWrite for HandshakeDeadlineStream<S>
where
    S: AsyncRdpStream,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // A client that is waiting for our bytes has, by definition, begun
        // the exchange; disarm on first successful write as well.
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(_)) = result {
            this.armed = false;
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

// No manual `AsyncRdpStream` impl: the blanket impl in `listener` covers the
// wrapper once it is `Unpin` + `AsyncRead` + `AsyncWrite`, which holds for
// every `S: AsyncRdpStream` (boxed `Sleep`, see struct).

/// Log helper used by the dispatcher when the deadline fires.
pub fn log_deadline_rejection(peer: &str, elapsed: Duration) {
    debug!(
        peer,
        elapsed_ms = elapsed.as_millis() as u64,
        "Handshake deadline elapsed before first client byte — dropping silent connection"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};

    async fn silent_peer() -> (DuplexStream, DuplexStream) {
        tokio::io::duplex(64)
    }

    #[tokio::test(start_paused = true)]
    async fn silent_peer_times_out_on_first_read() {
        let (_client_holds_open, server_side) = silent_peer().await;
        let mut wrapped = HandshakeDeadlineStream::new(server_side, Duration::from_secs(30));

        // Hold the socket open (client end alive in the tuple binding) but
        // never write. Advance past the deadline; the first read must error.
        tokio::time::advance(Duration::from_secs(31)).await;
        let err = wrapped
            .read(&mut [0u8; 8])
            .await
            .expect_err("must time out");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn bytes_within_deadline_disarm_it_forever() {
        use tokio::io::AsyncWriteExt as _;

        let (mut client, server_side) = silent_peer().await;
        let mut wrapped = HandshakeDeadlineStream::new(server_side, Duration::from_secs(30));

        client.write_all(&[0x03, 0x00]).await.unwrap();
        let n = wrapped.read(&mut [0u8; 8]).await.unwrap();
        assert!(n > 0);

        // Long idle after successful exchange: far past the original
        // deadline, reads must hang (Pending), not error.
        tokio::time::advance(Duration::from_secs(600)).await;
        let mut buf = [0u8; 8];
        let read_fut = wrapped.read(&mut buf);
        tokio::pin!(read_fut);
        let polled = tokio::time::timeout(Duration::from_millis(50), &mut read_fut).await;
        assert!(polled.is_err(), "idle read must stay pending, not time out");
    }

    #[tokio::test(start_paused = true)]
    async fn write_also_disarms() {
        use tokio::io::AsyncWriteExt as _;

        let (_client, server_side) = silent_peer().await;
        let mut wrapped = HandshakeDeadlineStream::new(server_side, Duration::from_secs(30));

        // Server speaks first (X.224 cases where we emit before reading).
        wrapped.write_all(&[1, 2, 3]).await.unwrap();
        tokio::time::advance(Duration::from_secs(31)).await;
        // Now the read side must be plain-Pending on silent client, not dead-
        // line-erroring, because the exchange already started.
        let mut buf = [0u8; 8];
        let read_fut = wrapped.read(&mut buf);
        tokio::pin!(read_fut);
        let polled = tokio::time::timeout(Duration::from_millis(50), &mut read_fut).await;
        assert!(polled.is_err(), "post-write read must stay pending");
    }
}
