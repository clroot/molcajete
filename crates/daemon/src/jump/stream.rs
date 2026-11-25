//! The connection to the remote server, direct or through jump hosts.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use russh::ChannelStream;
use russh::client::Msg;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::ssh::client::SshHandle;

/// A byte stream to the remote server: a TCP socket, or a `direct-tcpip`
/// channel of the last jump host. The SSH sessions of every hop live as
/// long as the stream.
pub enum Upstream {
    Tcp(TcpStream),
    Tunnel(Box<Tunnel>),
}

pub struct Tunnel {
    channel: ChannelStream<Msg>,
    /// Kept so that the hops stay connected; first hop first.
    _hops: Vec<SshHandle>,
}

/// Read half of an [`Upstream`].
pub type UpstreamReader = Box<dyn AsyncRead + Send + Unpin>;
/// Write half of an [`Upstream`].
pub type UpstreamWriter = Box<dyn AsyncWrite + Send + Unpin>;

impl Upstream {
    pub(super) fn tunnel(channel: ChannelStream<Msg>, hops: Vec<SshHandle>) -> Self {
        Self::Tunnel(Box::new(Tunnel {
            channel,
            _hops: hops,
        }))
    }

    /// The local address of the TCP connection; unspecified for a tunnel,
    /// whose TCP connection to the target belongs to the jump host.
    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Tcp(tcp) => tcp.local_addr().ok(),
            Self::Tunnel(_) => None,
        }
        .unwrap_or_else(|| (Ipv4Addr::UNSPECIFIED, 0).into())
    }

    /// Splits the stream so that it can be read and written from different
    /// tasks.
    pub fn into_split(self) -> (UpstreamReader, UpstreamWriter) {
        match self {
            Self::Tcp(tcp) => {
                let (read, write) = tcp.into_split();
                (Box::new(read), Box::new(write))
            }
            tunnel => {
                let (read, write) = tokio::io::split(tunnel);
                (Box::new(read), Box::new(write))
            }
        }
    }
}

impl AsyncRead for Upstream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(tcp) => Pin::new(tcp).poll_read(cx, buf),
            Self::Tunnel(t) => Pin::new(&mut t.channel).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Upstream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(tcp) => Pin::new(tcp).poll_write(cx, buf),
            Self::Tunnel(t) => Pin::new(&mut t.channel).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(tcp) => Pin::new(tcp).poll_flush(cx),
            Self::Tunnel(t) => Pin::new(&mut t.channel).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(tcp) => Pin::new(tcp).poll_shutdown(cx),
            Self::Tunnel(t) => Pin::new(&mut t.channel).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(tcp) => Pin::new(tcp).poll_write_vectored(cx, bufs),
            Self::Tunnel(t) => Pin::new(&mut t.channel).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Tcp(tcp) => tcp.is_write_vectored(),
            Self::Tunnel(t) => t.channel.is_write_vectored(),
        }
    }
}
