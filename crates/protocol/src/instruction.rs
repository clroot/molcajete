use std::fmt::{self, Write as _};

/// A single, fully parsed Guacamole instruction.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Instruction {
    pub opcode: String,
    pub args: Vec<String>,
}

impl Instruction {
    pub fn new<O, I, S>(opcode: O, args: I) -> Self
    where
        O: Into<String>,
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            opcode: opcode.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// An instruction without arguments.
    pub fn bare(opcode: impl Into<String>) -> Self {
        Self {
            opcode: opcode.into(),
            args: Vec::new(),
        }
    }

    /// The argument at `index`, or an empty string if absent.
    pub fn arg(&self, index: usize) -> &str {
        self.args.get(index).map(String::as_str).unwrap_or("")
    }

    /// Appends the serialized form of this instruction to `out`.
    pub fn encode_into(&self, out: &mut String) {
        encode_element(out, &self.opcode);
        for arg in &self.args {
            out.push(',');
            encode_element(out, arg);
        }
        out.push(';');
    }

    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(
            self.opcode.len() + self.args.iter().map(|a| a.len() + 6).sum::<usize>() + 8,
        );
        self.encode_into(&mut out);
        out
    }
}

impl fmt::Display for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

/// Length of an element in codepoints. ASCII strings take the fast path
/// because their byte length equals their codepoint count.
#[inline]
pub fn element_length(value: &str) -> usize {
    if value.is_ascii() {
        value.len()
    } else {
        value.chars().count()
    }
}

/// Serializes a single element as `LENGTH.VALUE`.
#[inline]
pub fn encode_element(out: &mut String, value: &str) {
    let _ = write!(out, "{}.", element_length(value));
    out.push_str(value);
}

/// Builds an encoded instruction string directly, without intermediate
/// `Instruction` allocations.
///
/// ```
/// use molcajete_protocol::InstructionBuilder;
/// let s = InstructionBuilder::new("mouse").arg(10).arg(20).arg(1).finish();
/// assert_eq!(s, "5.mouse,2.10,2.20,1.1;");
/// ```
pub struct InstructionBuilder {
    out: String,
    scratch: String,
}

impl InstructionBuilder {
    pub fn new(opcode: &str) -> Self {
        let mut out = String::with_capacity(64);
        encode_element(&mut out, opcode);
        Self {
            out,
            scratch: String::new(),
        }
    }

    /// Appends any `Display` value as an argument.
    pub fn arg(mut self, value: impl fmt::Display) -> Self {
        self.scratch.clear();
        let _ = write!(self.scratch, "{value}");
        self.out.push(',');
        encode_element(&mut self.out, &self.scratch);
        self
    }

    /// Appends a string argument (like `arg`, but skips formatting).
    pub fn str_arg(mut self, value: &str) -> Self {
        self.out.push(',');
        encode_element(&mut self.out, value);
        self
    }

    pub fn args<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for value in values {
            self = self.str_arg(value.as_ref());
        }
        self
    }

    pub fn finish(mut self) -> String {
        self.out.push(';');
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_ascii() {
        let ins = Instruction::new("size", ["0", "1024", "768"]);
        assert_eq!(ins.encode(), "4.size,1.0,4.1024,3.768;");
    }

    #[test]
    fn encodes_codepoint_lengths() {
        // "한글" = 2 codepoints / 6 bytes, the emoji = 1 codepoint / 4 bytes
        let ins = Instruction::new("name", ["한글", "😀"]);
        assert_eq!(ins.encode(), "4.name,2.한글,1.😀;");
    }

    #[test]
    fn encodes_internal_opcode() {
        let ins = Instruction::new("", ["ping", "123"]);
        assert_eq!(ins.encode(), "0.,4.ping,3.123;");
    }

    #[test]
    fn builder_matches_instruction() {
        let built = InstructionBuilder::new("key").arg(0xFF0D).arg(1).finish();
        assert_eq!(built, Instruction::new("key", ["65293", "1"]).encode());
    }
}
