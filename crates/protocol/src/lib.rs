//! Implementation of the Apache Guacamole protocol.
//!
//! The Guacamole protocol is a text protocol made of `LENGTH.VALUE` elements
//! joined by `,` and terminated by `;`. Crucially, `LENGTH` is the number of
//! **Unicode codepoints**, not bytes.
//!
//! ```text
//! 4.size,1.0,4.1024,3.768;
//! ```
//!
//! This crate is shared by the server (guacd replacement), the gateway and the
//! wasm client, so it has no platform-specific dependencies.

mod instruction;
mod parser;
pub mod server;
mod status;
mod types;
mod version;
pub mod video;

pub use instruction::{Instruction, InstructionBuilder, element_length, encode_element};
pub use parser::{Limits, ParseError, Parser, RawInstruction, parse_all};
pub use status::Status;
pub use types::{CompositeMode, LineCap, LineJoin, MouseButtons, TransferFunction};
pub use version::ProtocolVersion;

/// Opcode of tunnel-internal instructions. These are consumed by the tunnel
/// layer and never forwarded to guacd.
///
/// - `0.,36.<uuid>;`   : tunnel UUID announcement (server → client, once after connecting)
/// - `0.,4.ping,<ts>;` : connection stability ping (echoed back by the server)
pub const INTERNAL_OPCODE: &str = "";
