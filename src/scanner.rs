//! A character cursor over the source with line/column/byte-offset tracking.
//!
//! SCSS's lexical grammar is context-sensitive (a `:` can begin a
//! declaration value or a pseudo-class selector), so the parser scans
//! characters directly rather than over a fixed token stream. This
//! cursor is the shared primitive it builds on.

/// A source position: 1-based line/column plus the 0-based **byte** offset of
/// the same point in the source text.
///
/// Line and column are what diagnostics print; `offset` is what a tool that
/// slices the source wants — a UTF-8 byte index directly usable as
/// `&source[pos.offset..]`, with no line-table walk and no char-vs-byte
/// ambiguity (dart-sass measures span lengths in bytes too, see
/// [`Error::length`](crate::Error::length)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    /// 1-based line.
    pub line: usize,
    /// 1-based column, counted in **characters** (what the snippet renderer
    /// draws a caret under).
    pub col: usize,
    /// 0-based **byte** offset into the source text. Meaningful only when
    /// [`is_known`](Self::is_known) — [`Pos::NONE`] carries `0`, which is also
    /// a real offset for the first byte of a file.
    pub offset: usize,
}

impl Pos {
    /// The "no position" sentinel. `line == 0` can never be a real 1-based
    /// line, so it reads unambiguously as "unknown" (the source map drops it).
    pub const NONE: Pos = Pos {
        line: 0,
        col: 0,
        offset: 0,
    };

    /// A position from its 1-based line/column and 0-based byte offset.
    pub const fn new(line: usize, col: usize, offset: usize) -> Pos {
        Pos { line, col, offset }
    }

    /// Whether this is a real recorded position rather than [`Pos::NONE`].
    pub fn is_known(self) -> bool {
        self.line != 0
    }
}

/// An immutable snapshot of the cursor, used for backtracking lookahead.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mark {
    pos: usize,
    byte: usize,
    line: usize,
    col: usize,
}

pub(crate) struct Scanner {
    chars: Vec<char>,
    pos: usize,
    /// The UTF-8 byte offset of `pos`, tracked alongside the char index so
    /// every [`Pos`] can carry a byte offset without a rescan.
    byte: usize,
    line: usize,
    col: usize,
}

impl Scanner {
    pub(crate) fn new(src: &str) -> Self {
        // A leading UTF-8 byte-order mark is part of the encoding, not the
        // document; dart-sass strips it before parsing so it never reaches the
        // output (and never triggers a spurious `@charset`).
        let stripped = src.strip_prefix('\u{FEFF}').unwrap_or(src);
        // Byte offsets index the source AS THE CALLER PASSED IT, so a
        // stripped BOM still counts: `&src[pos.offset..]` is valid either way.
        let bom = src.len() - stripped.len();
        Scanner {
            chars: stripped.chars().collect(),
            pos: 0,
            byte: bom,
            line: 1,
            col: 1,
        }
    }

    pub(crate) fn position(&self) -> Pos {
        Pos {
            line: self.line,
            col: self.col,
            offset: self.byte,
        }
    }

    pub(crate) fn mark(&self) -> Mark {
        Mark {
            pos: self.pos,
            byte: self.byte,
            line: self.line,
            col: self.col,
        }
    }

    /// The number of source **bytes** (UTF-8) consumed since `m` was taken.
    /// Used to size a diagnostic span's caret underline (dart-sass measures
    /// span lengths in bytes).
    pub(crate) fn byte_len_from(&self, m: Mark) -> usize {
        self.byte.saturating_sub(m.byte)
    }

    pub(crate) fn reset(&mut self, m: Mark) {
        self.pos = m.pos;
        self.byte = m.byte;
        self.line = m.line;
        self.col = m.col;
    }

    pub(crate) fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    pub(crate) fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    /// Where dart-sass blames a missing statement terminator.
    ///
    /// dart consumes whitespace (newlines included) while looking for the
    /// `;`, but its diagnostics still point at the end of the statement's own
    /// line — `@use "a" as t` followed by a rule on the next line reports at
    /// column 18, not at column 1 of line 2. Reproduce that by walking the
    /// whitespace run that ends at the cursor back to the last character with
    /// content, then forward again over just the spaces and tabs that trail
    /// it. With no whitespace before the cursor this is the cursor itself.
    pub(crate) fn statement_end_pos(&self) -> Pos {
        let cur = self.pos.min(self.chars.len());
        let mut i = cur;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while matches!(self.chars.get(i), Some(&c) if c == ' ' || c == '\t') {
            i += 1;
        }
        if i >= cur {
            return self.position();
        }
        // Re-derive the position of `i` by unwinding the cursor over the
        // characters between them.
        let mut line = self.line;
        let mut byte = self.byte;
        for &c in &self.chars[i..cur] {
            byte -= c.len_utf8();
            if c == '\n' {
                line -= 1;
            }
        }
        let mut bol = i;
        while bol > 0 && self.chars[bol - 1] != '\n' {
            bol -= 1;
        }
        Pos {
            line,
            col: i - bol + 1,
            offset: byte,
        }
    }

    /// The remaining unconsumed characters, for non-consuming lookahead.
    pub(crate) fn rest(&self) -> &[char] {
        let start = self.pos.min(self.chars.len());
        &self.chars[start..]
    }

    pub(crate) fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        if let Some(ch) = c {
            self.pos += 1;
            self.byte += ch.len_utf8();
            if ch == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
        }
        c
    }

    /// The source text consumed since `m` was taken (for verbatim capture,
    /// e.g. a plain CSS `@import` URL emitted unchanged).
    pub(crate) fn slice_from(&self, m: Mark) -> String {
        let start = m.pos.min(self.chars.len());
        let end = self.pos.min(self.chars.len());
        self.chars[start..end].iter().collect()
    }

    /// Consume `c` if it is next; report whether it was consumed.
    pub(crate) fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.bump();
            true
        } else {
            false
        }
    }
}
