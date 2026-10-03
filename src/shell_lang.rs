//! Bounded shell syntax. Expansion is data, never another round of parsing.
//!
//! This module owns all decoded bytes so its result borrows neither the input
//! line nor the variable store. There is no heap, subprocess, substitution,
//! globbing, redirection, or hidden word splitting.

pub const MAX_LINE_BYTES: usize = 512;
pub const MAX_ARENA_BYTES: usize = 512;
pub const MAX_WORDS: usize = 48;
pub const MAX_STAGES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span { start: u16, len: u16, expanded: bool }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stage { first: u8, count: u8 }

/// An owned pipeline of decoded UTF-8 words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pipeline {
    arena: [u8; MAX_ARENA_BYTES],
    arena_len: u16,
    words: [Span; MAX_WORDS],
    word_len: u8,
    stages: [Stage; MAX_STAGES],
    stage_len: u8,
}

impl Pipeline {
    fn empty() -> Self {
        Self {
            arena: [0; MAX_ARENA_BYTES], arena_len: 0,
            words: [Span { start: 0, len: 0, expanded: false }; MAX_WORDS], word_len: 0,
            stages: [Stage { first: 0, count: 0 }; MAX_STAGES], stage_len: 0,
        }
    }

    pub fn stage_count(&self) -> usize { self.stage_len as usize }

    /// Out of range stages have no words.
    pub fn word_count(&self, stage: usize) -> usize {
        if stage < self.stage_count() { self.stages[stage].count as usize } else { 0 }
    }

    pub fn word(&self, stage: usize, index: usize) -> Option<&str> {
        if stage >= self.stage_count() || index >= self.word_count(stage) { return None; }
        let span = self.words[self.stages[stage].first as usize + index];
        let bytes = &self.arena[span.start as usize..(span.start + span.len) as usize];
        // Input and variable values are &str. Escapes insert ASCII and all
        // other fragments preserve complete UTF-8 byte sequences.
        core::str::from_utf8(bytes).ok()
    }

    pub fn decoded_bytes(&self) -> usize { self.arena_len as usize }

    /// Expansion provenance lets the executor require literal operation
    /// names while allowing arbitrary variable data in argument positions.
    pub fn word_expanded(&self, stage: usize, index: usize) -> bool {
        if stage >= self.stage_count() || index >= self.word_count(stage) { return false; }
        self.words[self.stages[stage].first as usize + index].expanded
    }

    fn push_byte(&mut self, value: u8, offset: usize) -> Result<(), Error> {
        let at = self.arena_len as usize;
        if at == MAX_ARENA_BYTES { return Err(Error::new(ErrorKind::ArenaFull, offset)); }
        self.arena[at] = value;
        self.arena_len += 1;
        Ok(())
    }

    fn push_data(&mut self, value: &str, offset: usize) -> Result<(), Error> {
        let at = self.arena_len as usize;
        if value.len() > MAX_ARENA_BYTES - at { return Err(Error::new(ErrorKind::ArenaFull, offset)); }
        self.arena[at..at + value.len()].copy_from_slice(value.as_bytes());
        self.arena_len += value.len() as u16;
        Ok(())
    }

    fn finish_word(&mut self, start: u16, expanded: bool, offset: usize) -> Result<(), Error> {
        let at = self.word_len as usize;
        if at == MAX_WORDS { return Err(Error::new(ErrorKind::TooManyWords, offset)); }
        self.words[at] = Span { start, len: self.arena_len - start, expanded };
        self.word_len += 1;
        Ok(())
    }

    fn finish_stage(&mut self, first: u8, offset: usize) -> Result<(), Error> {
        if first == self.word_len { return Err(Error::new(ErrorKind::EmptyStage, offset)); }
        let at = self.stage_len as usize;
        if at == MAX_STAGES { return Err(Error::new(ErrorKind::TooManyStages, offset)); }
        self.stages[at] = Stage { first, count: self.word_len - first };
        self.stage_len += 1;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InputTooLong,
    ArenaFull,
    TooManyWords,
    TooManyStages,
    EmptyStage,
    UnclosedQuote,
    InvalidEscape,
    InvalidVariable,
    UndefinedVariable,
    UnsupportedOperator,
    UnexpectedNewline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    /// Byte offset in the source line, not an offset in expanded data.
    pub offset: usize,
}

impl Error {
    const fn new(kind: ErrorKind, offset: usize) -> Self { Self { kind, offset } }

    pub fn message(self) -> &'static str {
        match self.kind {
            ErrorKind::InputTooLong => "command exceeds 512 source bytes",
            ErrorKind::ArenaFull => "expanded words exceed 512 bytes",
            ErrorKind::TooManyWords => "pipeline exceeds 48 words",
            ErrorKind::TooManyStages => "pipeline exceeds 8 stages",
            ErrorKind::EmptyStage => "pipe requires a command on each side",
            ErrorKind::UnclosedQuote => "quote is not closed",
            ErrorKind::InvalidEscape => "invalid or unfinished escape",
            ErrorKind::InvalidVariable => "variable must be $NAME or ${NAME}",
            ErrorKind::UndefinedVariable => "variable is not defined",
            ErrorKind::UnsupportedOperator => "shell operator is not supported",
            ErrorKind::UnexpectedNewline => "use one pipeline per source line",
        }
    }
}

fn variable_start(byte: u8) -> bool { byte.is_ascii_alphabetic() || byte == b'_' }
fn variable_continue(byte: u8) -> bool { variable_start(byte) || byte.is_ascii_digit() }

/// Expand `$NAME` or `${NAME}` directly into the current word.
fn expand<'v>(
    line: &str, cursor: &mut usize, pipeline: &mut Pipeline,
    lookup: &impl Fn(&str) -> Option<&'v str>,
) -> Result<(), Error> {
    let bytes = line.as_bytes();
    let dollar = *cursor;
    *cursor += 1;
    if bytes.get(*cursor) == Some(&b'(') {
        return Err(Error::new(ErrorKind::UnsupportedOperator, dollar));
    }
    let braces = bytes.get(*cursor) == Some(&b'{');
    if braces { *cursor += 1; }
    let start = *cursor;
    if !bytes.get(start).copied().is_some_and(variable_start) {
        return Err(Error::new(ErrorKind::InvalidVariable, dollar));
    }
    while bytes.get(*cursor).copied().is_some_and(variable_continue) { *cursor += 1; }
    let end = *cursor;
    if braces {
        if bytes.get(*cursor) != Some(&b'}') { return Err(Error::new(ErrorKind::InvalidVariable, dollar)); }
        *cursor += 1;
    }
    let value = lookup(&line[start..end]).ok_or(Error::new(ErrorKind::UndefinedVariable, dollar))?;
    pipeline.push_data(value, dollar)
}

fn escaped(bytes: &[u8], cursor: &mut usize, double_quote: bool) -> Result<u8, Error> {
    let slash = *cursor;
    *cursor += 1;
    let byte = bytes.get(*cursor).copied().ok_or(Error::new(ErrorKind::InvalidEscape, slash))?;
    *cursor += 1;
    match byte {
        b'n' => Ok(b'\n'), b't' => Ok(b'\t'), b'r' => Ok(b'\r'),
        b'\\' | b'\'' | b'"' | b'$' => Ok(byte),
        b' ' | b'|' | b'&' | b';' | b'<' | b'>' | b'`' | b'#' if !double_quote => Ok(byte),
        _ => Err(Error::new(ErrorKind::InvalidEscape, slash)),
    }
}

/// Parse one line without allocation. Expansion values remain one literal
/// word even if they contain spaces, quotes, pipes, or substitution syntax.
pub fn parse<'v>(line: &str, lookup: impl Fn(&str) -> Option<&'v str>) -> Result<Pipeline, Error> {
    if line.len() > MAX_LINE_BYTES { return Err(Error::new(ErrorKind::InputTooLong, MAX_LINE_BYTES)); }
    let mut result = Pipeline::empty();
    let bytes = line.as_bytes();
    let first_nonspace = bytes.iter().position(|b| !b.is_ascii_whitespace());
    if first_nonspace.is_none() || first_nonspace.map(|i| bytes[i]) == Some(b'#') { return Ok(result); }
    let mut cursor = 0;
    let mut first_word = 0;
    let mut word_start = None;
    let mut word_expanded = false;
    // 0 = no quote, 1 = single quote, 2 = double quote.
    let mut quote = 0;
    let mut quote_offset = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if quote == 1 {
            if byte == b'\'' { quote = 0; cursor += 1; }
            else { result.push_byte(byte, cursor)?; cursor += 1; }
            continue;
        }
        if quote == 2 {
            match byte {
                b'"' => { quote = 0; cursor += 1; }
                b'\\' => {
                    let at = cursor;
                    let value = escaped(bytes, &mut cursor, true)?;
                    result.push_byte(value, at)?;
                }
                // Quoted substitution-looking text is literal. Only a name
                // following '$' requests variable expansion.
                b'$' if bytes.get(cursor + 1) == Some(&b'(') => {
                    result.push_byte(byte, cursor)?; cursor += 1;
                }
                b'$' => {
                    word_expanded = true;
                    expand(line, &mut cursor, &mut result, &lookup)?;
                }
                _ => { result.push_byte(byte, cursor)?; cursor += 1; }
            }
            continue;
        }
        match byte {
            b'\n' | b'\r' => return Err(Error::new(ErrorKind::UnexpectedNewline, cursor)),
            b' ' | b'\t' | 0x0b | 0x0c => {
                if let Some(start) = word_start.take() { result.finish_word(start, word_expanded, cursor)?; }
                word_expanded = false;
                cursor += 1;
            }
            b'|' => {
                if bytes.get(cursor + 1) == Some(&b'|') { return Err(Error::new(ErrorKind::UnsupportedOperator, cursor)); }
                if let Some(start) = word_start.take() { result.finish_word(start, word_expanded, cursor)?; }
                word_expanded = false;
                result.finish_stage(first_word, cursor)?;
                first_word = result.word_len;
                cursor += 1;
            }
            b'<' | b'>' => {
                // Angle operators are syntax only in a literal `where`'s
                // third word. They remain forbidden as redirection anywhere
                // else, and attached tokens such as `>file` are rejected.
                let in_position = word_start.is_none()
                    && result.word_len as usize == first_word as usize + 2;
                let is_where = if in_position {
                    let head = result.words[first_word as usize];
                    !head.expanded
                        && &result.arena[head.start as usize..(head.start + head.len) as usize] == b"where"
                } else { false };
                let equals = bytes.get(cursor + 1) == Some(&b'=');
                let end = cursor + 1 + usize::from(equals);
                let boundary = bytes.get(end).copied().is_none_or(|b| b.is_ascii_whitespace() || b == b'|');
                if !in_position || !is_where || !boundary {
                    return Err(Error::new(ErrorKind::UnsupportedOperator, cursor));
                }
                word_start = Some(result.arena_len);
                result.push_byte(byte, cursor)?;
                if equals { result.push_byte(b'=', cursor + 1)?; }
                cursor = end;
            }
            b'&' | b';' | b'`' => return Err(Error::new(ErrorKind::UnsupportedOperator, cursor)),
            b'\'' | b'"' => {
                word_start.get_or_insert(result.arena_len);
                quote = if byte == b'\'' { 1 } else { 2 };
                quote_offset = cursor;
                cursor += 1;
            }
            b'\\' => {
                word_start.get_or_insert(result.arena_len);
                let at = cursor;
                let value = escaped(bytes, &mut cursor, false)?;
                result.push_byte(value, at)?;
            }
            b'$' => {
                word_start.get_or_insert(result.arena_len);
                word_expanded = true;
                expand(line, &mut cursor, &mut result, &lookup)?;
            }
            _ => {
                word_start.get_or_insert(result.arena_len);
                result.push_byte(byte, cursor)?;
                cursor += 1;
            }
        }
    }
    if quote != 0 { return Err(Error::new(ErrorKind::UnclosedQuote, quote_offset)); }
    if let Some(start) = word_start { result.finish_word(start, word_expanded, line.len())?; }
    result.finish_stage(first_word, line.len())?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(line: &str) -> Result<Pipeline, Error> { parse(line, |_| None) }

    #[test]
    fn quotes_fragments_and_utf8_remain_exact_words() {
        let parsed = literal("echo pre\" two\"' parts' \"\" '沖縄の道' | count").unwrap();
        assert_eq!(parsed.stage_count(), 2);
        assert_eq!(parsed.word_count(0), 4);
        assert_eq!(parsed.word(0, 1), Some("pre two parts"));
        assert_eq!(parsed.word(0, 2), Some(""));
        assert_eq!(parsed.word(0, 3), Some("沖縄の道"));
        assert_eq!(parsed.word(1, 0), Some("count"));
        assert_eq!(parsed.word(0, 4), None);
        assert_eq!(parsed.word(8, 0), None);
        assert_eq!(parsed.word_count(8), 0);
    }

    #[test]
    fn expansion_cannot_inject_a_stage_or_an_extra_argument() {
        let attack = "alpha beta | halt ; `reboot` $(file remove x) \" '";
        let parsed = parse("echo $PAYLOAD | count", |name| if name == "PAYLOAD" { Some(attack) } else { None }).unwrap();
        assert_eq!(parsed.stage_count(), 2);
        assert_eq!(parsed.word_count(0), 2);
        assert_eq!(parsed.word(0, 1), Some(attack));
        assert!(!parsed.word_expanded(0, 0));
        assert!(parsed.word_expanded(0, 1));
        assert!(!parsed.word_expanded(1, 0));
        assert!(!parsed.word_expanded(9, 0));
        assert_eq!(parsed.word(1, 0), Some("count"));
    }

    #[test]
    fn expansion_is_nonrecursive_and_single_quotes_are_literal() {
        let parsed = parse("echo '$X' \"${X}!\" pre$EMPTY\"post\"", |name| match name {
            "X" => Some("$Y"), "EMPTY" => Some(""), "Y" => Some("wrong"), _ => None,
        }).unwrap();
        assert_eq!(parsed.word(0, 1), Some("$X"));
        assert_eq!(parsed.word(0, 2), Some("$Y!"));
        assert_eq!(parsed.word(0, 3), Some("prepost"));
        assert!(!parsed.word_expanded(0, 1));
        assert!(parsed.word_expanded(0, 2));
        assert!(parsed.word_expanded(0, 3));
    }

    #[test]
    fn escapes_decode_data_and_do_not_create_operators() {
        let parsed = literal("echo \"line\\nnext\\t\\\\\\\"\\'\\$\" a\\ b \\| \\; \\`").unwrap();
        assert_eq!(parsed.word(0, 1), Some("line\nnext\t\\\"'$"));
        assert_eq!(parsed.word(0, 2), Some("a b"));
        assert_eq!(parsed.word(0, 3), Some("|"));
        assert_eq!(parsed.word(0, 4), Some(";"));
        assert_eq!(parsed.word(0, 5), Some("`"));
        for line in [r"echo \", "echo \"\\q\""] {
            assert_eq!(literal(line).unwrap_err().kind, ErrorKind::InvalidEscape);
        }
    }

    #[test]
    fn unsupported_execution_syntax_fails_before_dispatch() {
        for line in ["echo a && halt", "echo a || halt", "echo x > file", "cat < file", "echo a;halt", "echo `halt`", "echo $(halt)"] {
            assert_eq!(literal(line).unwrap_err().kind, ErrorKind::UnsupportedOperator, "{line}");
        }
        assert_eq!(literal("echo a\nhalt").unwrap_err().kind, ErrorKind::UnexpectedNewline);
        assert_eq!(literal("echo 'a; | > < & `b`'").unwrap().word(0, 1), Some("a; | > < & `b`"));
        assert_eq!(literal("echo \"$(halt) `halt`\"").unwrap().word(0, 1), Some("$(halt) `halt`"));
    }

    #[test]
    fn angle_comparisons_require_exact_where_argument_position() {
        for operator in ["<", "<=", ">", ">="] {
            let line = format!("file list | where bytes {operator} 2 | count");
            let parsed = literal(&line).unwrap();
            assert_eq!(parsed.word(1, 2), Some(operator));
            assert!(!parsed.word_expanded(1, 2));
        }
        for line in ["echo hi > file", "file list > file", "where bytes >file", "where bytes> 1", "where bytes >> 1", "where bytes <=> 1", "where > == 1", "where bytes == 1 > file", "wherebytes bytes > 1"] {
            assert_eq!(literal(line).unwrap_err().kind, ErrorKind::UnsupportedOperator, "{line}");
        }
        assert_eq!(parse("$OP bytes > 1", |_| Some("where")).unwrap_err().kind, ErrorKind::UnsupportedOperator);
    }

    #[test]
    fn empty_stages_unclosed_quotes_and_variable_errors_have_offsets() {
        for line in ["| echo", "echo |", "echo | | count"] {
            assert_eq!(literal(line).unwrap_err().kind, ErrorKind::EmptyStage);
        }
        let error = literal("echo 'unterminated").unwrap_err();
        assert_eq!(error.kind, ErrorKind::UnclosedQuote);
        assert_eq!(error.offset, 5);
        assert_eq!(literal("echo $NOPE").unwrap_err().offset, 5);
        for line in ["echo $", "echo $1", "echo ${}", "echo ${x", "echo ${x-y}"] {
            assert_eq!(literal(line).unwrap_err().kind, ErrorKind::InvalidVariable);
        }
    }

    #[test]
    fn comments_are_whole_line_only_and_do_not_expand() {
        assert_eq!(literal(" \t# $UNKNOWN | halt").unwrap().stage_count(), 0);
        assert_eq!(literal("\t ").unwrap().stage_count(), 0);
        assert_eq!(literal("").unwrap().stage_count(), 0);
        let parsed = literal("echo # still data").unwrap();
        assert_eq!(parsed.word_count(0), 4);
        assert_eq!(parsed.word(0, 1), Some("#"));
    }

    #[test]
    fn every_capacity_accepts_the_boundary_and_rejects_the_next_item() {
        let source = "x".repeat(MAX_LINE_BYTES);
        assert_eq!(literal(&source).unwrap().decoded_bytes(), MAX_ARENA_BYTES);
        assert_eq!(literal(&(source + "x")).unwrap_err().kind, ErrorKind::InputTooLong);
        let expanded = "界".repeat(170) + "aa";
        assert_eq!(parse("$X", |_| Some(expanded.as_str())).unwrap().decoded_bytes(), 512);
        let too_long = expanded + "x";
        assert_eq!(parse("$X", |_| Some(too_long.as_str())).unwrap_err().kind, ErrorKind::ArenaFull);
        let words = ["x"; MAX_WORDS].join(" ");
        assert_eq!(literal(&words).unwrap().word_count(0), MAX_WORDS);
        assert_eq!(literal(&(words.clone() + "|<")).unwrap_err().kind, ErrorKind::UnsupportedOperator);
        assert_eq!(literal(&(words + " x")).unwrap_err().kind, ErrorKind::TooManyWords);
        let stages = ["x"; MAX_STAGES].join("|");
        assert_eq!(literal(&stages).unwrap().stage_count(), MAX_STAGES);
        assert_eq!(literal(&(stages + "|x")).unwrap_err().kind, ErrorKind::TooManyStages);
    }
}
