//! Entry points of the fuzz targets in `fuzz/` that reach into molcajeted.
//! Each takes arbitrary bytes, runs them through the code that reads them
//! from an untrusted party, and panics only on a bug.

use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use crate::handshake;
use crate::io::{GuacReader, GuacWriter, Output};
use crate::session::{Input, InputDecoder};

/// Runs a future that reads from memory, which never has to wait.
fn run<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("a future reading from memory waited"),
    }
}

/// `data` is what a client sends to molcajeted: the handshake, then input
/// instructions (keys, mouse, size, clipboard and `argv` streams), each
/// decoded as a session does.
pub fn client(data: &[u8]) {
    run(async {
        let mut reader = GuacReader::new(data);
        let mut writer = GuacWriter::new(tokio::io::sink());
        if handshake::read_select(&mut reader).await.is_err()
            || handshake::negotiate(&mut reader, &mut writer, crate::vnc::ARGS, "$id")
                .await
                .is_err()
        {
            return;
        }
        let mut decoder = InputDecoder::default();
        let mut out = Output::text();
        while let Ok(Some(instruction)) = reader.next().await {
            let input = Input {
                instruction,
                received: Instant::now(),
                from_viewer: false,
                control: None,
            };
            let _ = decoder.decode(&input, &mut out);
            out.clear();
        }
    });
}

/// `data` is what a VNC server sends (see `vnc::rfb::fuzz`).
pub fn rfb(data: &[u8]) {
    run(crate::vnc::rfb::fuzz(data));
}

/// `data` is what an SSH server's shell prints (see `ssh::fuzz::output`).
pub fn terminal(data: &[u8]) {
    crate::ssh::fuzz::output(data);
}
