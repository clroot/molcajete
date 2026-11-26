//! Instrumentation of the connection to the remote desktop server: how many
//! bytes it sends and how long its updates take to decode. Together with
//! the CPU time of molcajeted they show what taking pixels in costs, per
//! byte and per megapixel received.
//!
//! Like the display metrics, these are no-ops unless the telemetry listener
//! is enabled.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use metrics::{Counter, Histogram, counter, histogram};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// `molcajeted_remote_received_bytes_total{protocol}`: bytes read from the
/// remote server's socket (TLS records included for RDP).
pub fn received_bytes(protocol: &'static str) -> Counter {
    counter!("molcajeted_remote_received_bytes_total", "protocol" => protocol)
}

/// `molcajeted_remote_frames_total{protocol}`: frames the remote server
/// marked the end of. It stays at zero for a server that does not mark its
/// frames, whose updates are then cut wherever frame pacing falls.
pub fn frames(protocol: &'static str) -> Counter {
    counter!("molcajeted_remote_frames_total", "protocol" => protocol)
}

/// Why a session writes to the remote server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteCause {
    /// An answer to something the server sent (acknowledgements of frames,
    /// replies on channels, network measurements).
    Response,
    /// Keys and pointer events of the client.
    Input,
    /// Anything the session starts itself (clipboard, resizing, redraws).
    Other,
}

/// `molcajeted_remote_writes_total{protocol, cause}`: writes to the remote
/// server's connection. Each one is a TLS record and a system call of its
/// own, so their number per second shows what batching them would save.
pub struct Writes([Counter; 3]);

impl Writes {
    pub fn new(protocol: &'static str) -> Self {
        Self(["response", "input", "other"].map(|cause| {
            counter!("molcajeted_remote_writes_total", "protocol" => protocol, "cause" => cause)
        }))
    }

    pub fn written(&self, cause: WriteCause) {
        self.0[cause as usize].increment(1);
    }
}

/// `molcajeted_remote_decode_seconds{protocol}`: time to decode one message
/// from the remote server and apply it to the framebuffer.
pub struct DecodeTime(Histogram);

impl DecodeTime {
    pub fn new(protocol: &'static str) -> Self {
        Self(histogram!("molcajeted_remote_decode_seconds", "protocol" => protocol))
    }

    pub fn record(&self, elapsed: Duration) {
        self.0.record(elapsed.as_secs_f64());
    }
}

/// A stream that counts the bytes read from it; writes pass through.
pub struct CountingStream<S> {
    inner: S,
    received: Counter,
}

impl<S> CountingStream<S> {
    pub fn new(inner: S, received: Counter) -> Self {
        Self { inner, received }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CountingStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = poll {
            self.received
                .increment((buf.filled().len() - before) as u64);
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CountingStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn reads_are_counted_and_writes_pass_through() {
        let received = Arc::new(AtomicU64::new(0));
        let (near, mut far) = tokio::io::duplex(64);
        let mut stream = CountingStream::new(near, Counter::from_arc(Arc::clone(&received)));
        far.write_all(b"hello, world").await.unwrap();
        let mut hello = [0u8; 5];
        stream.read_exact(&mut hello).await.unwrap();
        assert_eq!(&hello, b"hello");

        stream.write_all(b"ping").await.unwrap();
        let mut ping = [0u8; 4];
        far.read_exact(&mut ping).await.unwrap();
        assert_eq!(&ping, b"ping");

        drop(far);
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b", world");
        // Written bytes are not counted.
        assert_eq!(received.load(Ordering::Relaxed), 12);
    }
}
