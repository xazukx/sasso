//! Source-line metadata carried through evaluation into the serializer.
//!
//! The parser reports positions as byte offsets; the serializer needs
//! LINES, because two of its rules are line-based: whether a comment
//! trails the declaration before it, and how far a multi-line custom
//! property value may be re-indented. The evaluator resolves offsets to
//! lines once, at the node boundary, and carries the result here.
//!
//! [`Pos`] lives here for the same reason: it is the resolved form of a byte
//! offset, which is all the rest of the crate ever wants to talk about.

/// Source-line metadata for the serializer's trailing-comment rule
/// (dart `_isTrailingComment`). `file` is an evaluator-interned source id
/// (0 = unknown, never trailing).
///
/// Every line and column here is **1-based**, like [`Pos`], with
/// `0` reserved for "unknown"; see the crate docs for the crate-wide rule.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SrcLines {
    pub file: u32,
    /// A comment's `/*` line; a block's opening-brace line.
    pub start: u32,
    /// The construct's last line (closing delimiter / end of value).
    pub end: u32,
    /// A comment's `/*` column, or a custom property's name column (0 when
    /// neither applies): two comments with the same file/lines/column are
    /// span-identical CLONES (the same file imported twice), which dart's
    /// span-containment branch rejects as trailing — distinct same-line
    /// comments differ in column and still join.
    pub col: u32,
    /// The source COLUMN of the mapped token's start (a selector's first
    /// character, a declaration/at-rule keyword, a comment's `/*`), used
    /// ONLY by source-map generation — never by the serializer. `0` when
    /// unknown. Distinct from [`Self::col`], which is overloaded for the
    /// trailing-comment / custom-property-reindent heuristics; this field is a
    /// pure additive carrier and changes no CSS output.
    pub start_col: u32,
    /// Source-map-only OVERRIDE for the mapped source file id (0 = fall back to
    /// [`Self::file`]). A bubbled `@media`/`@at-root` wrapper re-uses the
    /// enclosing rule's selector position for its mapping while keeping
    /// `file`/`start`/`end` at 0 so the trailing-comment heuristic stays
    /// disabled (dart maps the duplicated parent selector to the ORIGINAL rule's
    /// span). Like [`Self::start_col`] this changes no CSS output.
    pub map_file: u32,
    /// Source-map-only OVERRIDE for the mapped source line (0 = fall
    /// back to [`Self::start`]). Set together with [`Self::map_file`] on a
    /// bubbled-selector wrapper; see that field.
    pub map_line: u32,
}

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
