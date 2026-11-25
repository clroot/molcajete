use std::sync::Arc;
use std::time::Duration;

use russh::keys::PrivateKey;
use russh::server::{self, Auth, ChannelOpenHandle, Session};
use russh::{Channel, ChannelOpenFailure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;

/// Host key of the test bastions.
const HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACBBinZYSqv1dXHZLi4hbrHO012O2KyjQmQAqNFPz4DDcgAAAIjOs2q9zrNq
vQAAAAtzc2gtZWQyNTUxOQAAACBBinZYSqv1dXHZLi4hbrHO012O2KyjQmQAqNFPz4DDcg
AAAEAtP4OHe1JyS7AgC3vzXHpzKaEqRi5HM2+vGQmSpJPD10GKdlhKq/V1cdkuLiFusc7T
XY7YrKNCZACo0U/PgMNyAAAAAAECAwQF
-----END OPENSSH PRIVATE KEY-----
";
const HOST_PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEGKdlhKq/V1cdkuLiFusc7TXY7YrKNCZACo0U/PgMNy";
const OTHER_PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIO96c7xdMKI1bXREpWNz3B+hvwc7QNP3y1qsCAjhZlV1";

const TIMEOUT: Duration = Duration::from_secs(10);

fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> {
    move |name| {
        pairs
            .iter()
            .find(|(n, v)| *n == name && !v.is_empty())
            .map(|(_, v)| *v)
    }
}

fn chain(pairs: &[(&str, &str)]) -> Chain {
    Chain::from_lookup(lookup(pairs)).unwrap()
}

#[test]
fn hop_syntax() {
    let parse = |s| parse_hop(s).unwrap();
    assert_eq!(parse("bastion"), (None, "bastion".into(), 22));
    assert_eq!(
        parse("admin@bastion:2222"),
        (Some("admin".into()), "bastion".into(), 2222)
    );
    assert_eq!(
        parse("me@corp@10.0.0.1"),
        (Some("me@corp".into()), "10.0.0.1".into(), 22)
    );
    assert_eq!(parse("[::1]:2200"), (None, "::1".into(), 2200));
    assert_eq!(parse("[fe80::1]"), (None, "fe80::1".into(), 22));
    assert_eq!(parse("fe80::1"), (None, "fe80::1".into(), 22));
    for bad in [
        "@host",
        "host:",
        "host:0",
        "host:99999",
        "[::1",
        "[::1]x",
        "a b",
    ] {
        assert!(parse_hop(bad).is_err(), "{bad}");
    }
    assert_eq!(address("::1", 22), "[::1]:22");
}

#[test]
fn no_jump_hosts_by_default() {
    let empty = chain(&[("jump-host-password", "unused")]);
    assert!(empty.hops().is_empty());
    assert!(empty.missing().is_empty());
}

#[test]
fn shared_values_and_per_hop_overrides() {
    let c = chain(&[
        ("jump-hosts", "a@one, two:2222\n\nb@three"),
        ("jump-host-username", "shared"),
        ("jump-host-password", "pw"),
        ("jump-host-private-key", "KEY"),
        ("jump-host-host-key", OTHER_PUBLIC_KEY),
        ("jump-host-2-password", "pw2"),
        ("jump-host-3-host-key", HOST_PUBLIC_KEY),
        ("jump-host-3-passphrase", "phrase"),
    ]);
    let hops = c.hops();
    assert_eq!(hops.len(), 3);
    assert_eq!(hops[0].username.as_deref(), Some("a"));
    assert_eq!(hops[1].username.as_deref(), Some("shared"));
    assert_eq!((hops[1].host.as_str(), hops[1].port), ("two", 2222));
    assert_eq!(hops[0].password.as_deref(), Some("pw"));
    assert_eq!(hops[0].private_key.as_deref(), Some("KEY"));
    // A hop's own password replaces the shared key as well.
    assert_eq!(hops[1].password.as_deref(), Some("pw2"));
    assert_eq!(hops[1].private_key, None);
    assert_eq!(hops[1].host_key.as_deref(), Some(OTHER_PUBLIC_KEY));
    assert_eq!(hops[2].host_key.as_deref(), Some(HOST_PUBLIC_KEY));
    assert_eq!(hops[2].passphrase.as_deref(), Some("phrase"));
    assert!(c.missing().is_empty());
    // Secrets stay out of logs.
    let debug = format!("{c:?}");
    for secret in ["pw", "pw2", "KEY", "phrase"] {
        assert!(!debug.contains(&format!("{secret:?}")), "{debug}");
    }
}

#[test]
fn invalid_parameters_are_refused() {
    let too_many = Chain::from_lookup(lookup(&[("jump-hosts", "a,b,c,d,e")]));
    assert!(too_many.unwrap_err().contains("at most 4"));
    let bad_key = Chain::from_lookup(lookup(&[
        ("jump-hosts", "u@bastion"),
        ("jump-host-host-key", "not a key"),
    ]));
    assert!(bad_key.unwrap_err().contains("bastion"));
    assert!(Chain::from_lookup(lookup(&[("jump-hosts", "u@:22")])).is_err());
}

#[test]
fn missing_credentials_are_asked_for() {
    let mut single = chain(&[("jump-hosts", "bastion")]);
    assert_eq!(
        single.missing(),
        ["jump-host-username", "jump-host-password"]
    );
    assert!(single.set("jump-host-username", "alice".into()));
    assert!(single.set("jump-host-password", "secret".into()));
    assert!(!single.set("password", "target's".into()));
    assert!(single.missing().is_empty());
    assert_eq!(single.hops()[0].username.as_deref(), Some("alice"));
    assert_eq!(single.hops()[0].password.as_deref(), Some("secret"));

    let mut two = chain(&[
        ("jump-hosts", "a@one,b@two"),
        ("jump-host-1-private-key", "KEY"),
    ]);
    assert_eq!(two.missing(), ["jump-host-2-password"]);
    assert!(two.set("jump-host-2-password", "pw".into()));
    assert!(two.missing().is_empty());
    assert_eq!(two.hops()[1].password.as_deref(), Some("pw"));
}

// ---------------------------------------------------------------------------------------------
// Against in-process SSH servers
// ---------------------------------------------------------------------------------------------

/// An SSH server that lets `jump`/`pw` in and forwards `direct-tcpip`
/// channels over TCP, like `sshd` with `AllowTcpForwarding yes`.
#[derive(Clone)]
struct Bastion;

impl server::Handler for Bastion {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "jump" && password == "pw" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<server::Msg>,
        host: &str,
        port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        match TcpStream::connect((host, port as u16)).await {
            Ok(mut tcp) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut channel = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut channel, &mut tcp).await;
                });
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }
}

/// Starts a bastion on a free port of 127.0.0.1.
async fn bastion() -> u16 {
    let config = Arc::new(server::Config {
        keys: vec![PrivateKey::from_openssh(HOST_KEY).unwrap()],
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let config = Arc::clone(&config);
            tokio::spawn(async move {
                if let Ok(session) = server::run_stream(config, tcp, Bastion).await {
                    let _ = session.await;
                }
            });
        }
    });
    port
}

/// A TCP server that answers every connection with `hello` and echoes the rest.
async fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                tcp.write_all(b"hello").await.unwrap();
                let (mut read, mut write) = tcp.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    port
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

async fn connect(chain: &Chain, port: u16) -> Result<Upstream, UpstreamError> {
    tokio::time::timeout(TIMEOUT, chain.connect("127.0.0.1", port))
        .await
        .expect("connecting timed out")
}

/// Checks that `upstream` reaches the echo server.
async fn assert_echoes(upstream: Upstream) {
    let (mut read, mut write) = upstream.into_split();
    let mut hello = [0; 5];
    tokio::time::timeout(TIMEOUT, read.read_exact(&mut hello))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&hello, b"hello");
    write.write_all(b"ping").await.unwrap();
    let mut ping = [0; 4];
    tokio::time::timeout(TIMEOUT, read.read_exact(&mut ping))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&ping, b"ping");
}

#[tokio::test]
async fn direct_without_jump_hosts() {
    let target = echo_server().await;
    let upstream = connect(&Chain::default(), target).await.unwrap();
    assert!(matches!(upstream, Upstream::Tcp(_)));
    assert_echoes(upstream).await;
}

#[tokio::test]
async fn through_one_jump_host() {
    let (bastion, target) = (bastion().await, echo_server().await);
    let hops = format!("jump@127.0.0.1:{bastion}");
    let chain = chain(&[
        ("jump-hosts", &hops),
        ("jump-host-password", "pw"),
        ("jump-host-host-key", HOST_PUBLIC_KEY),
    ]);
    let upstream = connect(&chain, target).await.unwrap();
    assert!(matches!(upstream, Upstream::Tunnel(_)));
    assert!(upstream.local_addr().ip().is_unspecified());
    assert_echoes(upstream).await;
}

#[tokio::test]
async fn through_two_jump_hosts() {
    let (first, second, target) = (bastion().await, bastion().await, echo_server().await);
    let hops = format!("jump@127.0.0.1:{first}, jump@127.0.0.1:{second}");
    let chain = chain(&[("jump-hosts", &hops), ("jump-host-password", "pw")]);
    assert_echoes(connect(&chain, target).await.unwrap()).await;
}

#[tokio::test]
async fn wrong_host_key_is_refused() {
    let (first, second) = (bastion().await, bastion().await);
    let hops = format!("jump@127.0.0.1:{first},jump@127.0.0.1:{second}");
    let chain = chain(&[
        ("jump-hosts", &hops),
        ("jump-host-password", "pw"),
        ("jump-host-1-host-key", HOST_PUBLIC_KEY),
        ("jump-host-2-host-key", OTHER_PUBLIC_KEY),
    ]);
    let err = connect(&chain, echo_server().await).await.err().unwrap();
    assert!(matches!(err, UpstreamError::HostKeyMismatch(_)), "{err}");
    let message = err.to_string();
    assert!(
        message.starts_with(&format!("jump host 2 (jump@127.0.0.1:{second})")),
        "{message}"
    );
    assert!(message.contains("host key"), "{message}");
    assert_eq!(err.status(), Status::UPSTREAM_ERROR);
}

#[tokio::test]
async fn wrong_password_is_unauthorized() {
    let bastion = bastion().await;
    let hops = format!("jump@127.0.0.1:{bastion}");
    let chain = chain(&[("jump-hosts", &hops), ("jump-host-password", "nope")]);
    let err = connect(&chain, echo_server().await).await.err().unwrap();
    assert_eq!(err.status(), Status::CLIENT_UNAUTHORIZED, "{err}");
    assert!(err.to_string().contains("jump host 1"), "{err}");
}

#[tokio::test]
async fn unreachable_hops_and_targets_are_named() {
    let closed = closed_port();
    let hops = format!("jump@127.0.0.1:{closed}");
    let chain1 = chain(&[("jump-hosts", &hops), ("jump-host-password", "pw")]);
    let err = connect(&chain1, echo_server().await).await.err().unwrap();
    assert!(
        err.to_string().starts_with(&format!(
            "cannot reach jump host 1 (jump@127.0.0.1:{closed})"
        )),
        "{err}"
    );
    assert_eq!(err.status(), Status::UPSTREAM_NOT_FOUND);

    // The second hop and the target are reached by the hop before them.
    let bastion = bastion().await;
    let hops = format!("jump@127.0.0.1:{bastion},jump@127.0.0.1:{closed}");
    let chain2 = chain(&[("jump-hosts", &hops), ("jump-host-password", "pw")]);
    let err = connect(&chain2, echo_server().await).await.err().unwrap();
    let expected = format!(
        "jump host 1 (jump@127.0.0.1:{bastion}) cannot open a connection to 127.0.0.1:{closed}"
    );
    assert!(err.to_string().starts_with(&expected), "{err}");
    assert_eq!(err.status(), Status::UPSTREAM_NOT_FOUND);

    let hops = format!("jump@127.0.0.1:{bastion}");
    let chain3 = chain(&[("jump-hosts", &hops), ("jump-host-password", "pw")]);
    let err = connect(&chain3, closed).await.err().unwrap();
    assert!(matches!(err, UpstreamError::Forward(..)), "{err}");
}

#[tokio::test]
async fn hop_without_username_is_refused() {
    let bastion = bastion().await;
    let hops = format!("127.0.0.1:{bastion}");
    let chain = chain(&[("jump-hosts", &hops), ("jump-host-password", "pw")]);
    let err = connect(&chain, echo_server().await).await.err().unwrap();
    assert!(matches!(err, UpstreamError::NoUsername(_)), "{err}");
}
