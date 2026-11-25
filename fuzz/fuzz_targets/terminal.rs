//! molcajeted's SSH terminal: arbitrary output of the remote shell fed to
//! alacritty_terminal and drawn by the renderer, with link detection and
//! selection.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    molcajete_daemon::fuzzing::terminal(data);
});
