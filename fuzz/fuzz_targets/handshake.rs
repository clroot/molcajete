//! molcajeted reading a client: the Guacamole handshake (`select`, the
//! client's `size` / `audio` / `video` / `image` / ... and `connect`), then
//! input instructions decoded as a session decodes them.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    molcajete_daemon::fuzzing::client(data);
});
