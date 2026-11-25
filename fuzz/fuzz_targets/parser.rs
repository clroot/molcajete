//! The Guacamole instruction parser: what the browser sends to the gateway
//! and molcajeted (strict limits), and what the server sends to the client
//! (lenient limits), cut anywhere.
//!
//! Input: one byte picking the limits and the split, then the stream. The
//! stream parsed in one piece and in pieces must give the same instructions
//! and end the same way; every instruction must survive encoding and
//! parsing again, and is decoded as a server instruction as the client does.

#![no_main]

use libfuzzer_sys::fuzz_target;
use molcajete_fuzz::pieces;
use molcajete_protocol::server::ServerInstruction;
use molcajete_protocol::{Instruction, Limits, ParseError, Parser, parse_all};

fn parse(limits: Limits, pieces: &[&[u8]]) -> (Vec<Instruction>, Option<ParseError>) {
    let mut parser = Parser::with_limits(limits);
    let mut instructions = Vec::new();
    for piece in pieces {
        parser.feed(piece);
        loop {
            match parser.next_raw() {
                Ok(Some(raw)) => {
                    let _ = ServerInstruction::decode(&raw);
                    assert_eq!(raw.as_str().len(), raw.as_bytes().len());
                    instructions.push(raw.to_instruction());
                }
                Ok(None) => break,
                Err(err) => return (instructions, Some(err)),
            }
        }
    }
    (instructions, None)
}

fuzz_target!(|data: &[u8]| {
    let Some((&seed, stream)) = data.split_first() else {
        return;
    };
    let limits = if seed & 1 == 0 {
        Limits::STRICT
    } else {
        Limits::LENIENT
    };
    let whole = parse(limits, &[stream]);
    let split = parse(limits, &pieces(seed >> 1, stream));
    assert_eq!(whole, split);
    for instruction in &whole.0 {
        let _ = ServerInstruction::decode(&instruction);
        assert_eq!(
            parse_all(&instruction.encode()).as_deref(),
            Ok(std::slice::from_ref(instruction))
        );
    }
});
