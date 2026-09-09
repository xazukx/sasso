//! Parsed source files.
//!
//! sasso does not parse SCSS itself: [`oxc_css_parser`] owns the whole grammar
//! and the evaluator walks its AST directly. This module is the seam — it
//! parses a file into the compile's arena and keeps everything the evaluator
//! needs to talk about a position in it.
//!
//! Every field is arena-backed, so a [`SourceFile`] is [`Copy`] and its
//! address is stable for the whole compile: the evaluator can hold `&'a`
//! references into a file's tree while still loading more files.

use oxc_css_parser::token::{Comment, CommentKind};
use oxc_css_parser::{Allocator, ParserBuilder, Span, Syntax as OxcSyntax, ast as ox};

use crate::error::Error;
use crate::scanner::Pos;

/// One parsed stylesheet, with the context needed to blame a span on it.
#[derive(Clone, Copy)]
pub(crate) struct SourceFile<'a> {
    /// The file's URL as it appears in diagnostics.
    pub url: &'a str,
    /// The BOM-stripped text. Every [`Span`] in [`Self::sheet`] indexes into
    /// this.
    pub text: &'a str,
    /// Bytes of BOM stripped off the front. Added back into a [`Pos`]'s byte
    /// offset so it indexes the text as the embedder passed it.
    bom: usize,
    /// Byte offset of the start of each line, for offset -> line/column.
    line_starts: &'a [usize],
    /// The file's loud (`/* … */`) comments, in source order. Silent (`//`)
    /// comments are dropped: dart-sass never emits them.
    ///
    /// Comments are not statements in the parser's AST; the evaluator
    /// re-interleaves these into each block by span (see [`interleave`]).
    pub comments: &'a [Comment<'a>],
    /// The grammar the file was parsed under. Re-parsing an interpolation
    /// body out of it has to use the same one.
    syntax: OxcSyntax,
}

impl<'a> SourceFile<'a> {
    /// Parse `text` under `syntax` into `arena`.
    ///
    /// `text` and `url` are interned into the arena, so the caller's copies do
    /// not have to outlive the result.
    /// The tree is returned separately, and mutably: it is the caller's to
    /// rewrite before evaluation (the staged API's whole point), while the
    /// file metadata around it stays shared and [`Copy`].
    pub(crate) fn parse(
        arena: &'a Allocator,
        url: &str,
        text: &str,
        syntax: OxcSyntax,
    ) -> Result<(&'a SourceFile<'a>, &'a mut ox::Stylesheet<'a>), Error> {
        // The parser strips a leading BOM and reports offsets into the
        // stripped text; sasso's positions index the text as the embedder
        // passed it, so the stripped bytes are added back in `pos`.
        let stripped = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        let bom = text.len() - stripped.len();

        let text: &'a str = arena.alloc_str(stripped);
        let url: &'a str = arena.alloc_str(url);

        // Comments are collected so loud ones can be re-emitted as output;
        // recoverable errors are collected because dart-sass rejects the
        // inputs the parser recovers from (see `check`).
        let mut parser = ParserBuilder::new(arena, text)
            .syntax(syntax)
            .comments()
            .build();

        let mut line_starts = vec![0usize];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        let line_starts: &'a [usize] = arena.alloc_slice_copy(&line_starts);

        // A syntax error needs a position, and positions come from the line
        // table — which exists before the tree does.
        let blame = |err: &oxc_css_parser::error::Error| {
            syntax_error(url, text, bom, line_starts, err)
        };

        let sheet = parser.parse::<ox::Stylesheet<'a>>().map_err(|e| blame(&e))?;
        // Recoverable errors are diagnostics the parser raised while still
        // building a usable tree. dart-sass rejects the same inputs outright,
        // so the first one is surfaced rather than dropped: the parser's
        // recovery exists to keep a tree walkable for a formatter, not to
        // widen what a compiler accepts.
        if let Some(first) = parser.recoverable_errors().first() {
            return Err(blame(first));
        }

        let comments: &'a [Comment<'a>] = arena.alloc_slice_copy(
            &parser
                .comments()
                .iter()
                .filter(|c| c.kind == CommentKind::Block)
                .copied()
                .collect::<Vec<_>>(),
        );
        Ok((
            arena.alloc(SourceFile {
                url,
                text,
                bom,
                line_starts,
                comments,
                syntax,
            }),
            arena.alloc(sheet),
        ))
    }

    /// The 1-based line/column and byte offset of `offset`.
    pub(crate) fn pos_at(&self, offset: usize) -> Pos {
        let line = self.line_starts.partition_point(|&s| s <= offset).max(1);
        let bol = self.line_starts[line - 1];
        // dart-sass counts a column in characters, not bytes.
        let col = self.text.get(bol..offset).map_or(0, |s| s.chars().count()) + 1;
        Pos::new(line, col, offset + self.bom)
    }

    /// The position a span starts at.
    pub(crate) fn pos(&self, span: Span) -> Pos {
        self.pos_at(span.start)
    }

    /// The source text of `span`, verbatim.
    pub(crate) fn text_of(&self, span: Span) -> &'a str {
        self.text.get(span.start..span.end).unwrap_or("")
    }

    /// The 1-based line `offset` sits on.
    pub(crate) fn line_at(&self, offset: usize) -> u32 {
        self.line_starts.partition_point(|&s| s <= offset).max(1) as u32
    }

    /// The grammar this file was parsed under.
    pub(crate) fn syntax(&self) -> OxcSyntax {
        self.syntax
    }

    /// Turn a parser error into a compiler error blamed on this file.
    pub(crate) fn syntax_error(&self, err: &oxc_css_parser::error::Error) -> Error {
        syntax_error(self.url, self.text, self.bom, self.line_starts, err)
    }
}

/// Turn a parser error into a compiler error blamed on the file the line table
/// describes.
///
/// The parser owns the grammar, so it also owns the wording of a syntax error:
/// its messages are surfaced as written rather than re-phrased to match
/// dart-sass.
fn syntax_error(
    url: &str,
    text: &str,
    bom: usize,
    line_starts: &[usize],
    err: &oxc_css_parser::error::Error,
) -> Error {
    let start = err.span.start.min(text.len());
    let end = err.span.end.min(text.len()).max(start);
    let line = line_starts.partition_point(|&s| s <= start).max(1);
    let bol = line_starts[line - 1];
    let col = text.get(bol..start).map_or(0, |s| s.chars().count()) + 1;
    let mut e = Error::at(err.kind.to_string(), Pos::new(line, col, start + bom));
    e.set_length(end - start);
    e.with_file(url.to_string())
}

/// One entry of a block walked in source order.
pub(crate) enum Item<'a, 'b> {
    /// A loud comment, which the evaluator emits as output.
    Comment(&'b Comment<'a>),
    Stmt(&'b ox::Statement<'a>),
}

/// Walk a block's statements in source order with its loud comments spliced
/// back in.
///
/// The parser reports comments out of band, so the interleaving is done by
/// span: every comment that starts before the next statement, and within
/// `span`, belongs to this block. It is derived fresh from the spans each time
/// rather than consumed from a cursor, so re-executing a block (a `@for` body,
/// a mixin included twice) emits its comments every time.
///
/// A comment nested inside a child statement is skipped here and picked up
/// when that child's own block is walked.
pub(crate) fn interleave<'a, 'b>(
    file: &SourceFile<'a>,
    stmts: &'b [ox::Statement<'a>],
    span: Span,
) -> Vec<Item<'a, 'b>> {
    let comments = file.comments;
    // The first comment that could fall inside this block.
    let mut c = comments.partition_point(|c| c.span.start < span.start);

    let mut out = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        let start = stmt.span().start;
        while c < comments.len() && comments[c].span.start < start {
            out.push(Item::Comment(&comments[c]));
            c += 1;
        }
        out.push(Item::Stmt(stmt));
        // Everything inside the statement belongs to one of its own blocks.
        let end = stmt.span().end;
        while c < comments.len() && comments[c].span.start < end {
            c += 1;
        }
    }
    while c < comments.len() && comments[c].span.end <= span.end {
        out.push(Item::Comment(&comments[c]));
        c += 1;
    }
    out
}
