//! Plain text from a terminal byte stream, for pattern and quiet period checks.

/// Longest partial line kept, in characters. A TUI that never prints a
/// newline keeps only its latest text.
const MAX_LINE_CHARS: usize = 4096;

/// Feeds output chunks through a `vte` parser and keeps the printable text:
/// escape sequences are removed, complete lines come out of
/// [`OutputScanner::feed`], and the current partial line stays readable.
/// A sequence (or a UTF-8 character) split across chunks is handled.
///
/// A carriage return not followed by a newline starts the line again, the
/// way a terminal overwrites it.
pub struct OutputScanner {
    parser: vte::Parser,
    text: Text,
    last_output_ms: Option<i64>,
}

#[derive(Default)]
struct Text {
    line: String,
    chars: usize,
    /// A `\r` was seen; the next printed character starts the line again.
    carriage: bool,
    done: Vec<String>,
}

impl Text {
    fn end_line(&mut self) {
        self.done.push(std::mem::take(&mut self.line));
        self.chars = 0;
        self.carriage = false;
    }

    fn push(&mut self, c: char) {
        if self.carriage {
            self.line.clear();
            self.chars = 0;
            self.carriage = false;
        }
        if self.chars == MAX_LINE_CHARS {
            let first = self.line.chars().next().map_or(0, char::len_utf8);
            self.line.drain(..first);
            self.chars -= 1;
        }
        self.line.push(c);
        self.chars += 1;
    }
}

impl vte::Perform for Text {
    fn print(&mut self, c: char) {
        self.push(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.end_line(),
            b'\r' => self.carriage = true,
            b'\t' => self.push(' '),
            _ => {}
        }
    }
}

impl Default for OutputScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputScanner {
    /// A scanner that has seen nothing.
    pub fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
            text: Text::default(),
            last_output_ms: None,
        }
    }

    /// Feed one chunk seen at `now_ms`. Returns the lines it completed, with
    /// escapes removed.
    pub fn feed(&mut self, chunk: &[u8], now_ms: i64) -> Vec<String> {
        if !chunk.is_empty() {
            self.last_output_ms = Some(now_ms);
        }
        self.parser.advance(&mut self.text, chunk);
        std::mem::take(&mut self.text.done)
    }

    /// The current partial line (text since the last newline).
    pub fn partial(&self) -> &str {
        &self.text.line
    }

    /// When output last arrived, if ever.
    pub fn last_output_ms(&self) -> Option<i64> {
        self.last_output_ms
    }

    /// True when output has arrived and none came in the last `period_ms`.
    pub fn quiet_since(&self, now_ms: i64, period_ms: i64) -> bool {
        self.last_output_ms
            .is_some_and(|at| now_ms.saturating_sub(at) >= period_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_should_strip_escapes_and_yield_complete_lines() {
        let mut s = OutputScanner::new();
        let lines = s.feed(b"\x1b[1;32mhello\x1b[0m world\r\nnext", 1);
        assert_eq!(
            (lines, s.partial()),
            (vec!["hello world".to_owned()], "next")
        );
    }

    #[test]
    fn an_escape_split_across_chunks_should_still_be_removed() {
        let mut s = OutputScanner::new();
        let mut lines = s.feed(b"Allow \x1b[3", 1);
        lines.extend(s.feed(b"1mrm\x1b[0m? [y/n] ", 2));
        assert_eq!((lines.len(), s.partial()), (0, "Allow rm? [y/n] "));
    }

    #[test]
    fn a_utf8_character_split_across_chunks_should_survive() {
        let mut s = OutputScanner::new();
        let bytes = "café\n".as_bytes();
        let mut lines = s.feed(&bytes[..4], 1);
        lines.extend(s.feed(&bytes[4..], 2));
        assert_eq!(lines, vec!["café".to_owned()]);
    }

    #[test]
    fn a_bare_carriage_return_should_overwrite_the_line() {
        let mut s = OutputScanner::new();
        s.feed(b"working 10%\rworking 90%", 1);
        assert_eq!(s.partial(), "working 90%");
    }

    #[test]
    fn quiet_since_should_follow_the_last_output_time() {
        let mut s = OutputScanner::new();
        let before = s.quiet_since(10_000, 1000);
        s.feed(b"x", 5_000);
        s.feed(b"", 5_900);
        assert_eq!(
            (
                before,
                s.quiet_since(5_999, 1000),
                s.quiet_since(6_000, 1000)
            ),
            (false, false, true)
        );
    }

    #[test]
    fn a_window_title_sequence_should_leave_no_text_behind() {
        let mut s = OutputScanner::new();
        let lines = s.feed(b"\x1b]0;claude: working\x07ready\r\n", 1);
        assert_eq!(lines, vec!["ready".to_owned()]);
    }

    #[test]
    fn a_tab_should_read_as_a_space() {
        let mut s = OutputScanner::new();
        s.feed(b"Allow?\t[y/n]", 1);
        assert_eq!(s.partial(), "Allow? [y/n]");
    }

    #[test]
    fn a_clock_stepping_back_should_never_read_as_quiet() {
        let mut s = OutputScanner::new();
        s.feed(b"x", 5_000);
        assert!(!s.quiet_since(1_000, 1_000));
    }

    #[test]
    fn a_line_with_no_newline_should_keep_only_its_latest_text() {
        let mut s = OutputScanner::new();
        s.feed(&vec![b'a'; MAX_LINE_CHARS], 1);
        s.feed(b"end", 2);
        assert!(s.partial().chars().count() == MAX_LINE_CHARS && s.partial().ends_with("aend"));
    }
}
