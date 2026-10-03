//! A bounded, allocation-free ASCII line editor shared by serial and PS/2 input.
//!
//! This module owns no terminal: every visible edit or cursor move returns
//! `Changed`, leaving the caller to redraw. `Submit` remembers the current line
//! but leaves it available until `clear()` so command execution can borrow it.

pub const LINE_CAPACITY: usize = 255;
pub const HISTORY_CAPACITY: usize = 8;
const ESCAPE_CAPACITY: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event { None, Changed, Submit, Cancel, Complete, Full }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Character(u8), Left, Right, Home, End, Backspace, Delete, Up, Down,
    Submit, Cancel, Complete, ClearBefore, ClearWord, ClearAfter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetLineError { TooLong, NonAscii }

#[derive(Clone, Copy)]
struct Line { bytes: [u8; LINE_CAPACITY], len: usize }

impl Line {
    const EMPTY: Self = Self { bytes: [0; LINE_CAPACITY], len: 0 };
    fn text(&self) -> &str {
        // All writers accept printable ASCII only. No unchecked UTF-8 is used.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape { Ground, Prefix, Csi, Ss3, String, StringEscape }

pub struct Editor {
    current: Line,
    cursor: usize,
    history: [Line; HISTORY_CAPACITY],
    history_len: usize,
    /// An index into oldest-first history; None means the draft is active.
    selected: Option<usize>,
    draft: Line,
    draft_cursor: usize,
    escape: Escape,
    parameters: [u8; ESCAPE_CAPACITY],
    parameter_len: usize,
    damaged: bool,
    swallow_lf: bool,
    overflowed: bool,
}

impl Editor {
    pub const fn new() -> Self {
        Self {
            current: Line::EMPTY, cursor: 0,
            history: [Line::EMPTY; HISTORY_CAPACITY], history_len: 0,
            selected: None, draft: Line::EMPTY, draft_cursor: 0,
            escape: Escape::Ground, parameters: [0; ESCAPE_CAPACITY],
            parameter_len: 0, damaged: false, swallow_lf: false, overflowed: false,
        }
    }

    pub fn line(&self) -> &str { self.current.text() }
    pub fn length(&self) -> usize { self.current.len }
    pub fn cursor(&self) -> usize { self.cursor }
    /// Once an input byte exceeded capacity, the entire command is rejected.
    /// The caller checks this on Submit; only clear/Cancel starts a safe draft.
    pub fn overflowed(&self) -> bool { self.overflowed }
    pub fn history_len(&self) -> usize { self.history_len }
    /// Erase saved privileged input when the shell permanently lowers domain.
    pub fn clear_history(&mut self) {
        self.history = [Line::EMPTY; HISTORY_CAPACITY];
        self.history_len = 0;
        self.selected = None;
        self.draft = Line::EMPTY;
        self.draft_cursor = 0;
    }
    /// Return an entry in oldest-first order. Stored history is never edited.
    pub fn history(&self, index: usize) -> Option<&str> {
        if index < self.history_len { Some(self.history[index].text()) } else { None }
    }

    /// Replace the visible line atomically, preserving history and navigation.
    /// Completion callers should redraw on success; the cursor moves to the end.
    pub fn set_line(&mut self, text: &str) -> Result<(), SetLineError> {
        if text.len() > LINE_CAPACITY { return Err(SetLineError::TooLong); }
        if !text.bytes().all(|b| (b' '..=b'~').contains(&b)) {
            return Err(SetLineError::NonAscii);
        }
        self.current.bytes[..text.len()].copy_from_slice(text.as_bytes());
        self.current.len = text.len();
        self.cursor = text.len();
        Ok(())
    }

    /// Start a fresh draft after execution, retaining history and CR/LF pairing.
    pub fn clear(&mut self) {
        self.current.len = 0;
        self.cursor = 0;
        self.selected = None;
        self.draft.len = 0;
        self.draft_cursor = 0;
        self.overflowed = false;
        self.reset_escape();
    }

    fn reset_escape(&mut self) {
        self.escape = Escape::Ground;
        self.parameter_len = 0;
        self.damaged = false;
    }

    /// Decode fragmented ANSI input. Invalid sequences are consumed, and CSI
    /// sequences exceeding the fixed budget are discarded through their final
    /// byte. Newline/Ctrl-C recover from a truncated sequence without inserting
    /// its parameter bytes into the command.
    pub fn feed(&mut self, byte: u8) -> Event {
        if self.swallow_lf {
            self.swallow_lf = false;
            if byte == b'\n' { return Event::None; }
        }
        if byte == 3 {
            self.reset_escape();
            return self.feed_key(Key::Cancel);
        }
        if byte == b'\r' || byte == b'\n' {
            self.reset_escape();
            self.swallow_lf = byte == b'\r';
            return self.feed_key(Key::Submit);
        }
        if byte == 0x1b {
            if matches!(self.escape, Escape::String | Escape::StringEscape) { self.escape = Escape::StringEscape; }
            else { self.escape = Escape::Prefix; }
            self.parameter_len = 0;
            self.damaged = false;
            return Event::None;
        }
        match self.escape {
            Escape::Prefix => {
                self.escape = match byte {
                    b'[' => Escape::Csi,
                    b'O' => Escape::Ss3,
                    b']' | b'P' | b'^' | b'_' => Escape::String,
                    _ => Escape::Ground,
                };
                return Event::None;
            }
            Escape::Ss3 => {
                self.reset_escape();
                return self.ansi_key(byte, &[]);
            }
            Escape::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    let mut parameters = [0; ESCAPE_CAPACITY];
                    let len = self.parameter_len;
                    parameters[..len].copy_from_slice(&self.parameters[..len]);
                    let damaged = self.damaged;
                    self.reset_escape();
                    return if damaged { Event::None } else { self.ansi_key(byte, &parameters[..len]) };
                }
                if self.parameter_len == ESCAPE_CAPACITY {
                    self.damaged = true;
                } else {
                    self.parameters[self.parameter_len] = byte;
                    self.parameter_len += 1;
                    if !(0x20..=0x3f).contains(&byte) { self.damaged = true; }
                }
                return Event::None;
            }
            Escape::String => {
                if byte == 7 { self.reset_escape(); }
                return Event::None;
            }
            Escape::StringEscape => {
                if byte == b'\\' { self.reset_escape(); }
                else { self.escape = Escape::String; }
                return Event::None;
            }
            Escape::Ground => {}
        }
        let key = match byte {
            1 => Key::Home, 2 => Key::Left, 4 => Key::Delete,
            5 => Key::End, 6 => Key::Right, 8 | 127 => Key::Backspace,
            9 => Key::Complete, 11 => Key::ClearAfter,
            14 => Key::Down, 16 => Key::Up,
            21 => Key::ClearBefore, 23 => Key::ClearWord,
            b' '..=b'~' => Key::Character(byte),
            _ => return Event::None,
        };
        self.feed_key(key)
    }

    fn ansi_key(&mut self, final_byte: u8, parameters: &[u8]) -> Event {
        let key = match (final_byte, parameters) {
            (b'A', b"") => Key::Up, (b'B', b"") => Key::Down,
            (b'C', b"") => Key::Right, (b'D', b"") => Key::Left,
            (b'H', b"") | (b'~', b"1") | (b'~', b"7") => Key::Home,
            (b'F', b"") | (b'~', b"4") | (b'~', b"8") => Key::End,
            (b'~', b"3") => Key::Delete,
            _ => return Event::None,
        };
        self.feed_key(key)
    }

    pub fn feed_key(&mut self, key: Key) -> Event {
        match key {
            Key::Character(byte) => {
                if !(b' '..=b'~').contains(&byte) { return Event::None; }
                if self.current.len == LINE_CAPACITY {
                    self.overflowed = true;
                    return Event::Full;
                }
                self.current.bytes.copy_within(self.cursor..self.current.len, self.cursor + 1);
                self.current.bytes[self.cursor] = byte;
                self.cursor += 1;
                self.current.len += 1;
            }
            Key::Left if self.cursor > 0 => self.cursor -= 1,
            Key::Right if self.cursor < self.current.len => self.cursor += 1,
            Key::Home if self.cursor > 0 => self.cursor = 0,
            Key::End if self.cursor < self.current.len => self.cursor = self.current.len,
            Key::Backspace if self.cursor > 0 => self.remove(self.cursor - 1, self.cursor),
            Key::Delete if self.cursor < self.current.len => self.remove(self.cursor, self.cursor + 1),
            Key::ClearBefore if self.cursor > 0 => self.remove(0, self.cursor),
            Key::ClearAfter if self.cursor < self.current.len => self.remove(self.cursor, self.current.len),
            Key::ClearWord if self.cursor > 0 => {
                let mut start = self.cursor;
                while start > 0 && self.current.bytes[start - 1] == b' ' { start -= 1; }
                while start > 0 && self.current.bytes[start - 1] != b' ' { start -= 1; }
                self.remove(start, self.cursor);
            }
            Key::Up => return self.previous(),
            Key::Down => return self.next(),
            Key::Submit => {
                if !self.overflowed { self.remember(); }
                return Event::Submit;
            }
            Key::Cancel => { self.clear(); self.swallow_lf = false; return Event::Cancel; }
            Key::Complete => return Event::Complete,
            _ => return Event::None,
        }
        Event::Changed
    }

    fn remove(&mut self, start: usize, end: usize) {
        self.current.bytes.copy_within(end..self.current.len, start);
        self.current.len -= end - start;
        self.cursor = start;
    }

    fn previous(&mut self) -> Event {
        if self.history_len == 0 { return Event::None; }
        let index = match self.selected {
            None => {
                self.draft = self.current;
                self.draft_cursor = self.cursor;
                self.history_len - 1
            }
            Some(0) => return Event::None,
            Some(index) => index - 1,
        };
        self.selected = Some(index);
        self.current = self.history[index];
        self.cursor = self.current.len;
        Event::Changed
    }

    fn next(&mut self) -> Event {
        match self.selected {
            None => return Event::None,
            Some(index) if index + 1 < self.history_len => {
                self.selected = Some(index + 1);
                self.current = self.history[index + 1];
                self.cursor = self.current.len;
            }
            Some(_) => {
                self.selected = None;
                self.current = self.draft;
                self.cursor = self.draft_cursor;
            }
        }
        Event::Changed
    }

    fn remember(&mut self) {
        if self.current.text().trim_ascii().is_empty() { return; }
        if self.history_len > 0 && self.history[self.history_len - 1].text() == self.line() { return; }
        if self.history_len == HISTORY_CAPACITY {
            self.history.copy_within(1..HISTORY_CAPACITY, 0);
            self.history_len -= 1;
        }
        self.history[self.history_len] = self.current;
        self.history_len += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(editor: &mut Editor, bytes: &[u8]) { for &byte in bytes { editor.feed(byte); } }
    fn remember(editor: &mut Editor, text: &str) {
        editor.set_line(text).unwrap();
        assert_eq!(editor.feed_key(Key::Submit), Event::Submit);
        editor.clear();
    }

    #[test]
    fn insert_move_delete_and_backspace() {
        let mut e = Editor::new();
        input(&mut e, b"ac\x1b[Db");
        assert_eq!((e.line(), e.cursor()), ("abc", 2));
        assert_eq!(e.feed_key(Key::Backspace), Event::Changed);
        assert_eq!((e.line(), e.cursor()), ("ac", 1));
        input(&mut e, b"\x1b[3~");
        assert_eq!(e.line(), "a");
        assert_eq!(e.feed_key(Key::Delete), Event::None);
        input(&mut e, b"\x01z\x05!");
        assert_eq!((e.line(), e.cursor()), ("za!", 3));
    }

    #[test]
    fn word_and_line_controls_keep_the_suffix() {
        let mut e = Editor::new();
        input(&mut e, b"one two  three");
        e.feed_key(Key::Left); e.feed_key(Key::Left);
        e.feed(23);
        assert_eq!((e.line(), e.cursor()), ("one two  ee", 9));
        e.feed(23);
        assert_eq!((e.line(), e.cursor()), ("one ee", 4));
        e.feed(21);
        assert_eq!((e.line(), e.cursor()), ("ee", 0));
        e.feed(11);
        assert_eq!(e.line(), "");
        assert_eq!(e.feed(23), Event::None);
    }

    #[test]
    fn home_end_variants_and_fragmented_sequences() {
        let mut e = Editor::new();
        input(&mut e, b"abc");
        for seq in [b"\x1b[H".as_slice(), b"\x1b[1~", b"\x1bOH", b"\x1b[7~"] {
            input(&mut e, seq); assert_eq!(e.cursor(), 0);
            input(&mut e, b"\x1b[F"); assert_eq!(e.cursor(), 3);
        }
        input(&mut e, b"\x1b[H");
        for seq in [b"\x1b[4~".as_slice(), b"\x1bOF", b"\x1b[8~"] {
            input(&mut e, seq); assert_eq!(e.cursor(), 3); e.feed(1);
        }
        e.feed(0x1b); assert_eq!(e.feed(b'['), Event::None);
        assert_eq!(e.feed(b'C'), Event::Changed);
        assert_eq!(e.cursor(), 1);
        assert_eq!(e.line(), "abc");
    }

    #[test]
    fn capacity_is_explicit_and_set_line_is_atomic() {
        let mut e = Editor::new();
        for _ in 0..LINE_CAPACITY { assert_eq!(e.feed(b'x'), Event::Changed); }
        assert_eq!(e.feed(b'y'), Event::Full);
        e.feed_key(Key::Home);
        assert_eq!(e.feed(b'y'), Event::Full);
        assert_eq!(e.length(), LINE_CAPACITY);
        let long = "z".repeat(LINE_CAPACITY + 1);
        assert_eq!(e.set_line(&long), Err(SetLineError::TooLong));
        assert_eq!(e.cursor(), 0);
        assert_eq!(e.set_line("日本語"), Err(SetLineError::NonAscii));
        assert_eq!(e.set_line("a\nb"), Err(SetLineError::NonAscii));
        assert_eq!(e.length(), LINE_CAPACITY);
        assert_eq!(e.feed(0xff), Event::None);
        assert_eq!(e.feed_key(Key::Character(0)), Event::None);
        e.feed_key(Key::Delete);
        assert_eq!(e.feed(b'y'), Event::Changed);
        assert!(e.line().starts_with('y'));
    }

    #[test]
    fn history_restores_draft_and_original_cursor() {
        let mut e = Editor::new();
        remember(&mut e, "first"); remember(&mut e, "second");
        input(&mut e, b"draft\x1b[D\x1b[D");
        input(&mut e, b"\x1b[A"); assert_eq!(e.line(), "second");
        input(&mut e, b"\x1b[A"); assert_eq!(e.line(), "first");
        assert_eq!(e.feed_key(Key::Up), Event::None);
        e.feed(b'!'); assert_eq!(e.line(), "first!");
        input(&mut e, b"\x1b[B\x1b[B");
        assert_eq!((e.line(), e.cursor()), ("draft", 3));
        assert_eq!(e.feed_key(Key::Down), Event::None);
        assert_eq!(e.history(0), Some("first"));
        assert_eq!(e.history(1), Some("second"));
        assert_eq!(e.history(2), None);
    }

    #[test]
    fn history_rolls_and_suppresses_only_adjacent_duplicates() {
        let mut e = Editor::new();
        for i in 0..10 { remember(&mut e, &i.to_string()); }
        assert_eq!(e.history_len(), HISTORY_CAPACITY);
        assert_eq!(e.history(0), Some("2"));
        remember(&mut e, "9"); remember(&mut e, " ");
        assert_eq!(e.history(0), Some("2"));
        remember(&mut e, "2");
        assert_eq!(e.history(0), Some("3"));
        assert_eq!(e.history(7), Some("2"));
        e.feed_key(Key::Up); e.feed_key(Key::Submit); e.clear();
        assert_eq!(e.history(0), Some("3"));
    }

    #[test]
    fn submit_retains_line_crlf_is_one_event_and_cancel_never_remembers() {
        let mut e = Editor::new();
        input(&mut e, b"echo one");
        assert_eq!(e.feed(b'\r'), Event::Submit);
        assert_eq!(e.line(), "echo one");
        assert_eq!(e.history_len(), 1);
        e.clear();
        assert_eq!(e.feed(b'\n'), Event::None);
        input(&mut e, b"discard\x1b[12");
        assert_eq!(e.feed(3), Event::Cancel);
        assert_eq!((e.line(), e.cursor(), e.history_len()), ("", 0, 1));
        assert_eq!(e.feed(b'\n'), Event::Submit);
        assert_eq!(e.history_len(), 1);
        e.clear(); e.feed_key(Key::Up);
        assert_eq!(e.line(), "echo one");
        assert_eq!(e.feed(3), Event::Cancel);
        assert_eq!(e.feed_key(Key::Down), Event::None);
        assert_eq!(e.line(), "");
    }

    #[test]
    fn malformed_overlong_and_terminal_strings_do_not_leak() {
        let mut e = Editor::new();
        input(&mut e, b"ok\x1b[999~\x1b[1;5D\x1bx\x1b[12345678901234567890D");
        assert_eq!((e.line(), e.cursor()), ("ok", 2));
        input(&mut e, b"\x1b[12\x01D!");
        assert_eq!((e.line(), e.cursor()), ("ok!", 3));
        input(&mut e, b"\x1b]title=delete\x07\x1bPdiscard\x1b\\.");
        assert_eq!(e.line(), "ok!.");
        input(&mut e, b"\x1b[123");
        assert_eq!(e.feed(b'\r'), Event::Submit);
        assert_eq!(e.line(), "ok!.");
        e.clear(); e.feed(b'\n'); e.feed(b'x');
        assert_eq!(e.line(), "x");
    }

    #[test]
    fn completion_is_a_request_without_mutating_input_or_history() {
        let mut e = Editor::new();
        input(&mut e, b"he");
        assert_eq!(e.feed(b'\t'), Event::Complete);
        assert_eq!((e.line(), e.cursor(), e.history_len()), ("he", 2, 0));
        e.set_line("help ").unwrap();
        assert_eq!((e.line(), e.cursor()), ("help ", 5));
    }

    #[test]
    fn rejected_overflow_cannot_become_a_truncated_valid_command() {
        let mut e = Editor::new();
        let valid_prefix = "echo ".to_owned() + &"x".repeat(LINE_CAPACITY - 5);
        e.set_line(&valid_prefix).unwrap();
        assert_eq!(e.feed(b'y'), Event::Full);
        assert!(e.overflowed());
        e.feed_key(Key::Backspace);
        assert!(e.overflowed());
        assert_eq!(e.feed(b'\r'), Event::Submit);
        assert!(e.overflowed());
        assert_eq!(e.history_len(), 0);
        e.clear();
        assert!(!e.overflowed());
        e.feed(b'\n'); input(&mut e, b"echo safe"); e.feed(b'\r');
        assert_eq!(e.history(0), Some("echo safe"));
        e.clear(); e.set_line(&valid_prefix).unwrap(); e.feed(b'y');
        assert_eq!(e.feed(3), Event::Cancel);
        assert!(!e.overflowed());
    }

    #[test]
    fn clear_history_removes_privileged_history_and_saved_draft() {
        let mut e = Editor::new();
        remember(&mut e, "write secret private");
        input(&mut e, b"draft secret");
        e.feed_key(Key::Up);
        e.set_line("drop").unwrap();
        e.clear_history();
        assert_eq!(e.history_len(), 0);
        assert_eq!(e.history(0), None);
        assert_eq!(e.feed_key(Key::Up), Event::None);
        assert_eq!(e.feed_key(Key::Down), Event::None);
        assert_eq!(e.line(), "drop");
        e.clear();
        assert_eq!(e.feed_key(Key::Down), Event::None);
        assert_eq!(e.line(), "");
    }

    #[test]
    fn arbitrary_byte_streams_preserve_editor_and_history_bounds() {
        let mut e = Editor::new();
        let mut random = 0x713e_8b91u32;
        for index in 0..30_000 {
            random ^= random << 13; random ^= random >> 17; random ^= random << 5;
            let event = e.feed(random as u8);
            assert!(e.length() <= LINE_CAPACITY && e.cursor() <= e.length());
            assert!(e.line().bytes().all(|byte| (b' '..=b'~').contains(&byte)));
            assert!(e.history_len() <= HISTORY_CAPACITY);
            for history_index in 0..e.history_len() {
                let line = e.history(history_index).unwrap();
                assert!(line.len() <= LINE_CAPACITY && line.is_ascii());
            }
            if event == Event::Submit { e.clear(); }
            if index % 97 == 0 { e.feed_key(Key::Up); }
            if index % 101 == 0 { e.feed_key(Key::Down); }
        }
    }
}
