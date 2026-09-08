//! The compiler error type.

use std::fmt;

use crate::scanner::Pos;

/// A compilation error, carrying a human-readable message and the source
/// position it is blamed on.
///
/// The position is available in both flavours: 1-based `line`/`col` for
/// display (`0` when unknown), and the 0-based byte range
/// [`start`](Error::start)`..`[`end`](Error::end) for a tool that slices the
/// source directly. [`file`](Error::file) names the file the position is in,
/// which is not necessarily the entry file — an error raised inside an
/// `@import`ed or `@use`d stylesheet is blamed on *that* file.
///
/// Implements [`std::error::Error`], so it composes with `?` and the
/// wider error ecosystem.
///
/// For byte-exact dart-sass diagnostics the evaluator attaches a span
/// `length` (in source bytes) and a [`rendered`](Error::rendered) snippet
/// block at the AST-node boundary. When `rendered` is present, [`Display`](std::fmt::Display)
/// emits it verbatim (the full `Error: …` + source-span snippet + stack
/// trace); otherwise it falls back to the legacy `Error: <msg> (line:col)`
/// one-liner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Human-readable description of what went wrong.
    pub message: String,
    /// 1-based line number, or `0` if unknown.
    pub line: usize,
    /// 1-based column number, or `0` if unknown.
    pub col: usize,
    /// 0-based **byte** offset of the first byte of the offending span, in the
    /// source of [`file`](Error::file). `0` when unknown — check
    /// [`has_position`](Error::has_position) first, exactly as for
    /// `line`/`col`.
    pub start: usize,
    /// 0-based **byte** offset one past the last byte of the offending span,
    /// i.e. the span is `start..end`. Equal to `start` for a zero-width span
    /// (the renderer still draws a single caret), so `end - start == length`.
    pub end: usize,
    /// Length of the offending span, in **bytes** of the source. `0` when the
    /// span length is unknown (the renderer still draws a single caret).
    pub length: usize,
    /// The URL of the file the position refers to, as it appears in
    /// diagnostics (the entry file's [`Options::url`](crate::Options::url), or
    /// the loaded file's URL for an error inside an `@import`/`@use`). `None`
    /// when no position is known, or when diagnostics carry no file name
    /// (`Options::url` unset).
    pub file: Option<String>,
    /// The fully rendered diagnostic block (header + snippet + frames), set by
    /// the evaluator once the source/url/glyph context is known. `None` until
    /// then; [`Display`](std::fmt::Display) falls back to the legacy one-liner.
    pub rendered: Option<String>,
}

impl Error {
    /// A positioned error: `message` blamed on the source position `pos`
    /// (1-based line/column plus its byte offset), with a zero-width span.
    pub fn at(message: impl Into<String>, pos: Pos) -> Self {
        Error {
            message: message.into(),
            line: pos.line,
            col: pos.col,
            start: pos.offset,
            end: pos.offset,
            length: 0,
            file: None,
            rendered: None,
        }
    }

    /// An error with no source position (rendered as the bare `Error: <msg>`).
    pub fn unpositioned(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            line: 0,
            col: 0,
            start: 0,
            end: 0,
            length: 0,
            file: None,
            rendered: None,
        }
    }

    /// Attach a span length (in source bytes) if one is not already set. Used at
    /// AST-node boundaries to size the caret underline; keeps
    /// [`end`](Error::end) in step.
    pub fn with_length(mut self, length: usize) -> Self {
        if self.length == 0 {
            self.set_length(length);
        }
        self
    }

    /// Blame `pos` (with `length` bytes of caret) if no position has been
    /// recorded yet, leaving an already positioned error untouched.
    ///
    /// This is how a position reaches an error raised deep in a helper that
    /// has no span of its own: the evaluator applies the enclosing AST node's
    /// position as each statement and expression boundary is unwound, so the
    /// innermost node that *has* a span wins — the same rule dart-sass gets
    /// from attaching a span to every node it evaluates.
    pub fn or_at(mut self, pos: Pos, length: usize) -> Self {
        if !self.has_position() && pos.is_known() {
            self.line = pos.line;
            self.col = pos.col;
            self.start = pos.offset;
            // `length` belongs to `pos`: applying it to an error that already
            // has a narrower span of its own would stretch the caret across
            // text the error is not about.
            if self.length == 0 {
                self.length = length;
            }
            self.end = self.start + self.length;
        }
        self
    }

    /// Attribute this error to `file` (a diagnostic URL) if it is not already
    /// attributed.
    pub fn with_file(mut self, file: impl Into<String>) -> Self {
        if self.file.is_none() {
            self.file = Some(file.into());
        }
        self
    }

    /// Set the span length and the derived [`end`](Error::end) together, so the
    /// two never drift. Prefer this over assigning
    /// [`length`](Error::length) directly.
    pub fn set_length(&mut self, length: usize) {
        self.length = length;
        self.end = self.start + length;
    }

    /// Whether a primary `line`/`col` position has been recorded. The byte
    /// offsets [`start`](Error::start)/[`end`](Error::end) are meaningful only
    /// when this is `true`.
    pub fn has_position(&self) -> bool {
        self.line > 0
    }

    /// The offending span as a byte range into the source of
    /// [`file`](Error::file), or `None` when no position is known.
    ///
    /// ```
    /// use sasso::{compile, Options};
    ///
    /// let src = "a { color: $missing; }";
    /// let err = compile(src, &Options::default().with_url("input.scss")).unwrap_err();
    /// assert_eq!(&src[err.byte_span().unwrap()], "$missing");
    /// ```
    pub fn byte_span(&self) -> Option<std::ops::Range<usize>> {
        self.has_position().then_some(self.start..self.end)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(r) = &self.rendered {
            return f.write_str(r);
        }
        if self.line > 0 {
            write!(f, "Error: {} ({}:{})", self.message, self.line, self.col)
        } else {
            write!(f, "Error: {}", self.message)
        }
    }
}

impl std::error::Error for Error {}
