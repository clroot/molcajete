//! molcajeted's RFB client reading a VNC server: the handshake, server
//! messages (framebuffer updates in every supported encoding, cursor,
//! resize, clipboard) and the ZRLE tile decoder.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    molcajete_daemon::fuzzing::rfb(data);
});
