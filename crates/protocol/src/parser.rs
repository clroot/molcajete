//! Incremental, byte-oriented Guacamole instruction parser.
//!
//! Data may arrive in arbitrary chunks (TCP reads from guacd, WebSocket
//! messages), possibly splitting an instruction or even a UTF-8 sequence. The
//! parser keeps its scanning state between calls so that each byte is examined
//! only once, regardless of how the input is fragmented.

use crate::Instruction;

/// Resource limits enforced while parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum number of bytes a single encoded instruction may occupy.
    pub max_instruction_bytes: usize,
    /// Maximum number of elements (opcode included) in one instruction.
    pub max_elements: usize,
    /// Maximum number of decimal digits in an element length prefix.
    pub max_length_digits: usize,
}

impl Limits {
    /// Limits equivalent to libguac / guacamole-common. Suitable for data
    /// coming from untrusted clients.
    pub const STRICT: Limits = Limits {
        max_instruction_bytes: 8192 * 4,
        max_elements: 128,
        max_length_digits: 5,
    };

    /// Generous limits for data coming from a trusted server.
    pub const LENIENT: Limits = Limits {
        max_instruction_bytes: 16 * 1024 * 1024,
        max_elements: 4096,
        max_length_digits: 9,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Limits::LENIENT
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("invalid byte {0:#04x} in element length")]
    InvalidLength(u8),
    #[error("element length prefix has too many digits")]
    LengthTooLong,
    #[error("unexpected byte {0:#04x} after element (expected ',' or ';')")]
    InvalidTerminator(u8),
    #[error("instruction exceeds {0} bytes")]
    InstructionTooLong(usize),
    #[error("instruction has more than {0} elements")]
    TooManyElements(usize),
    #[error("element is not valid UTF-8")]
    InvalidUtf8,
}

#[derive(Clone, Copy, Debug)]
enum State {
    /// Reading the decimal length prefix of an element.
    Length { value: usize, digits: usize },
    /// Reading the element value; `remaining` codepoints are still expected.
    Value {
        start: usize,
        remaining: usize,
        multibyte: bool,
    },
    /// The value is complete; expecting `,` or `;`.
    Terminator { start: usize, multibyte: bool },
}

const INITIAL_STATE: State = State::Length {
    value: 0,
    digits: 0,
};

/// Incremental Guacamole parser. Feed bytes with [`Parser::feed`] and pull
/// complete instructions with [`Parser::next_raw`] or
/// [`Parser::next_instruction`].
pub struct Parser {
    buf: Vec<u8>,
    /// Start offset of the instruction currently being scanned.
    start: usize,
    /// Next byte to examine.
    cursor: usize,
    state: State,
    /// Byte ranges (absolute offsets into `buf`) of the scanned elements of
    /// the current instruction.
    elements: Vec<(usize, usize)>,
    /// Set after an instruction has been handed out; the element list is
    /// reset lazily so that the returned `RawInstruction` can borrow it.
    completed: bool,
    limits: Limits,
    error: Option<ParseError>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    pub fn with_limits(limits: Limits) -> Self {
        Self {
            buf: Vec::with_capacity(8192),
            start: 0,
            cursor: 0,
            state: INITIAL_STATE,
            elements: Vec::with_capacity(16),
            completed: false,
            limits,
            error: None,
        }
    }

    /// Appends received bytes.
    pub fn feed(&mut self, data: &[u8]) {
        self.compact();
        self.buf.extend_from_slice(data);
    }

    /// Number of buffered bytes that do not yet form a complete instruction.
    pub fn pending_len(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Drops already consumed bytes from the front of the buffer. Only done
    /// when new data arrives so that draining many instructions from a single
    /// chunk never moves memory more than once.
    fn compact(&mut self) {
        if self.completed {
            self.elements.clear();
            self.completed = false;
        }
        let shift = self.start;
        if shift == 0 {
            return;
        }
        self.buf.drain(..shift);
        self.start = 0;
        self.cursor -= shift;
        for (s, e) in &mut self.elements {
            *s -= shift;
            *e -= shift;
        }
        match &mut self.state {
            State::Length { .. } => {}
            State::Value { start, .. } | State::Terminator { start, .. } => *start -= shift,
        }
    }

    /// Returns the next complete instruction as a zero-copy view into the
    /// internal buffer, `Ok(None)` if more data is needed.
    pub fn next_raw(&mut self) -> Result<Option<RawInstruction<'_>>, ParseError> {
        if let Some(err) = &self.error {
            return Err(err.clone());
        }
        if self.completed {
            self.elements.clear();
            self.completed = false;
        }
        match self.scan() {
            Ok(Some((begin, end))) => Ok(Some(RawInstruction {
                buf: &self.buf,
                range: (begin, end),
                elements: &self.elements,
            })),
            Ok(None) => Ok(None),
            Err(err) => {
                self.error = Some(err.clone());
                Err(err)
            }
        }
    }

    /// Returns the next complete instruction as an owned [`Instruction`].
    pub fn next_instruction(&mut self) -> Result<Option<Instruction>, ParseError> {
        Ok(self.next_raw()?.map(|raw| raw.to_instruction()))
    }

    /// Advances the state machine. Returns the byte range of a completed
    /// instruction.
    fn scan(&mut self) -> Result<Option<(usize, usize)>, ParseError> {
        let len = self.buf.len();
        loop {
            if self.cursor - self.start > self.limits.max_instruction_bytes {
                return Err(ParseError::InstructionTooLong(
                    self.limits.max_instruction_bytes,
                ));
            }
            match self.state {
                State::Length { value, digits } => {
                    let Some(&b) = self.buf.get(self.cursor) else {
                        return Ok(None);
                    };
                    match b {
                        b'0'..=b'9' => {
                            if digits + 1 > self.limits.max_length_digits {
                                return Err(ParseError::LengthTooLong);
                            }
                            self.state = State::Length {
                                value: value * 10 + usize::from(b - b'0'),
                                digits: digits + 1,
                            };
                            self.cursor += 1;
                        }
                        b'.' if digits > 0 => {
                            // Reject oversized elements before buffering them.
                            if self.cursor + 1 - self.start + value
                                > self.limits.max_instruction_bytes
                            {
                                return Err(ParseError::InstructionTooLong(
                                    self.limits.max_instruction_bytes,
                                ));
                            }
                            self.cursor += 1;
                            self.state = State::Value {
                                start: self.cursor,
                                remaining: value,
                                multibyte: false,
                            };
                        }
                        other => return Err(ParseError::InvalidLength(other)),
                    }
                }
                State::Value {
                    start,
                    mut remaining,
                    mut multibyte,
                } => {
                    while remaining > 0 {
                        let avail = len - self.cursor;
                        if avail == 0 {
                            break;
                        }
                        // Fast path: consume a run of ASCII bytes at once.
                        let n = avail.min(remaining);
                        let window = &self.buf[self.cursor..self.cursor + n];
                        let run = window.iter().position(|&b| b >= 0x80).unwrap_or(n);
                        self.cursor += run;
                        remaining -= run;
                        if remaining == 0 || self.cursor == len {
                            break;
                        }
                        // Slow path: a single multi-byte UTF-8 sequence.
                        let width =
                            utf8_width(self.buf[self.cursor]).ok_or(ParseError::InvalidUtf8)?;
                        let available = (len - self.cursor).min(width);
                        let continuation = &self.buf[self.cursor + 1..self.cursor + available];
                        if continuation.iter().any(|&b| b & 0xC0 != 0x80) {
                            return Err(ParseError::InvalidUtf8);
                        }
                        if available < width {
                            break;
                        }
                        self.cursor += width;
                        remaining -= 1;
                        multibyte = true;
                    }
                    if remaining > 0 {
                        self.state = State::Value {
                            start,
                            remaining,
                            multibyte,
                        };
                        return Ok(None);
                    }
                    self.state = State::Terminator { start, multibyte };
                }
                State::Terminator { start, multibyte } => {
                    let Some(&b) = self.buf.get(self.cursor) else {
                        return Ok(None);
                    };
                    if b != b',' && b != b';' {
                        return Err(ParseError::InvalidTerminator(b));
                    }
                    if multibyte && std::str::from_utf8(&self.buf[start..self.cursor]).is_err() {
                        return Err(ParseError::InvalidUtf8);
                    }
                    if self.elements.len() >= self.limits.max_elements {
                        return Err(ParseError::TooManyElements(self.limits.max_elements));
                    }
                    self.elements.push((start, self.cursor));
                    self.cursor += 1;
                    self.state = INITIAL_STATE;
                    if b == b';' {
                        let range = (self.start, self.cursor);
                        self.start = self.cursor;
                        self.completed = true;
                        return Ok(Some(range));
                    }
                }
            }
        }
    }
}

/// Width of a UTF-8 sequence given its leading byte, `None` if the byte cannot
/// start a sequence.
#[inline]
fn utf8_width(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

/// A zero-copy view of a complete instruction inside the parser buffer.
pub struct RawInstruction<'a> {
    buf: &'a [u8],
    range: (usize, usize),
    elements: &'a [(usize, usize)],
}

impl<'a> RawInstruction<'a> {
    fn element(&self, index: usize) -> Option<&'a str> {
        let &(s, e) = self.elements.get(index)?;
        // SAFETY: every element was validated as UTF-8 while scanning (either
        // it was pure ASCII or it passed `str::from_utf8`).
        Some(unsafe { std::str::from_utf8_unchecked(&self.buf[s..e]) })
    }

    pub fn opcode(&self) -> &'a str {
        self.element(0).unwrap_or("")
    }

    /// Number of arguments (opcode excluded).
    pub fn arg_count(&self) -> usize {
        self.elements.len().saturating_sub(1)
    }

    /// The argument at `index` (0-based, opcode excluded), or `""`.
    pub fn arg(&self, index: usize) -> &'a str {
        self.element(index + 1).unwrap_or("")
    }

    pub fn args(&self) -> impl Iterator<Item = &'a str> + '_ {
        (0..self.arg_count()).map(move |i| self.arg(i))
    }

    /// The complete encoded instruction, including the trailing `;`.
    pub fn as_str(&self) -> &'a str {
        // SAFETY: the instruction consists of validated UTF-8 elements and
        // ASCII separators/length prefixes.
        unsafe { std::str::from_utf8_unchecked(self.as_bytes()) }
    }

    pub fn as_bytes(&self) -> &'a [u8] {
        &self.buf[self.range.0..self.range.1]
    }

    pub fn to_instruction(&self) -> Instruction {
        Instruction {
            opcode: self.opcode().to_owned(),
            args: self.args().map(str::to_owned).collect(),
        }
    }
}

impl std::fmt::Debug for RawInstruction<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RawInstruction")
            .field(&self.as_str())
            .finish()
    }
}

/// Parses a string containing only complete instructions.
pub fn parse_all(input: &str) -> Result<Vec<Instruction>, ParseError> {
    let mut parser = Parser::new();
    parser.feed(input.as_bytes());
    let mut out = Vec::new();
    while let Some(ins) = parser.next_instruction()? {
        out.push(ins);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(parser: &mut Parser) -> Vec<Instruction> {
        let mut out = Vec::new();
        while let Some(ins) = parser.next_instruction().unwrap() {
            out.push(ins);
        }
        out
    }

    #[test]
    fn parses_multiple_instructions() {
        let all = parse_all("4.size,1.0,4.1024,3.768;4.sync,4.1234;").unwrap();
        assert_eq!(
            all,
            vec![
                Instruction::new("size", ["0", "1024", "768"]),
                Instruction::new("sync", ["1234"]),
            ]
        );
    }

    #[test]
    fn parses_empty_elements_and_internal_opcode() {
        let all = parse_all("0.,4.ping,1.5;3.nop;4.name,0.;").unwrap();
        assert_eq!(all[0], Instruction::new("", ["ping", "5"]));
        assert_eq!(all[1], Instruction::bare("nop"));
        assert_eq!(all[2], Instruction::new("name", [""]));
    }

    #[test]
    fn lengths_are_codepoints() {
        let all = parse_all("4.name,5.안녕하세요,1.😀,3.a¢b;").unwrap();
        assert_eq!(all[0].args, ["안녕하세요", "😀", "a¢b"]);
    }

    #[test]
    fn byte_by_byte_feeding_matches_bulk() {
        let input = "4.name,5.안녕하세요,1.😀;4.blob,1.1,8.AAAA/+==;3.end,1.1;".as_bytes();
        let expected = parse_all(std::str::from_utf8(input).unwrap()).unwrap();
        let mut parser = Parser::new();
        let mut got = Vec::new();
        for b in input {
            parser.feed(std::slice::from_ref(b));
            got.extend(collect(&mut parser));
        }
        assert_eq!(got, expected);
        assert_eq!(parser.pending_len(), 0);
    }

    #[test]
    fn every_split_point_works() {
        let input = "3.img,1.1,2.14,1.0,9.image/png,1.0,1.0;4.blob,1.1,3.한국어;3.end,1.1;";
        let expected = parse_all(input).unwrap();
        let bytes = input.as_bytes();
        for split in 0..=bytes.len() {
            let mut parser = Parser::new();
            parser.feed(&bytes[..split]);
            let mut got = collect(&mut parser);
            parser.feed(&bytes[split..]);
            got.extend(collect(&mut parser));
            assert_eq!(got, expected, "split at {split}");
        }
    }

    #[test]
    fn raw_view_exposes_original_text() {
        let mut parser = Parser::new();
        parser.feed(b"4.sync,4.1234;5.mouse,1.1");
        let raw = parser.next_raw().unwrap().unwrap();
        assert_eq!(raw.opcode(), "sync");
        assert_eq!(raw.arg(0), "1234");
        assert_eq!(raw.as_str(), "4.sync,4.1234;");
        assert!(parser.next_raw().unwrap().is_none());
        assert_eq!(parser.pending_len(), "5.mouse,1.1".len());
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_all("x.size;"), Err(ParseError::InvalidLength(b'x')));
        assert_eq!(
            parse_all("4.size:"),
            Err(ParseError::InvalidTerminator(b':'))
        );
        assert_eq!(parse_all(".;"), Err(ParseError::InvalidLength(b'.')));
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut parser = Parser::new();
        parser.feed(b"4.name,1.\xFF;");
        assert_eq!(parser.next_raw().unwrap_err(), ParseError::InvalidUtf8);
        let mut parser = Parser::new();
        // Leading byte of a 2-byte sequence followed by a non-continuation byte.
        parser.feed(b"4.name,2.\xC3a;");
        assert_eq!(parser.next_raw().unwrap_err(), ParseError::InvalidUtf8);
    }

    #[test]
    fn enforces_limits() {
        let limits = Limits {
            max_instruction_bytes: 16,
            max_elements: 3,
            max_length_digits: 2,
        };
        let mut p = Parser::with_limits(limits);
        p.feed(b"123.x");
        assert_eq!(p.next_raw().unwrap_err(), ParseError::LengthTooLong);

        let mut p = Parser::with_limits(limits);
        p.feed(b"1.a,1.b,1.c,1.d;");
        assert_eq!(p.next_raw().unwrap_err(), ParseError::TooManyElements(3));

        let mut p = Parser::with_limits(limits);
        p.feed(b"20.aaaaaaaaaaaaaaaaaaaa;");
        assert_eq!(
            p.next_raw().unwrap_err(),
            ParseError::InstructionTooLong(16)
        );
    }

    #[test]
    fn errors_are_sticky() {
        let mut p = Parser::new();
        p.feed(b"x;");
        assert!(p.next_raw().is_err());
        p.feed(b"3.nop;");
        assert!(p.next_raw().is_err());
    }
}
