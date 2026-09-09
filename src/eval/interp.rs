//! Resolving `#{…}` interpolation.
//!
//! Several constructs are text as far as the parser is concerned — a selector,
//! an at-rule prelude, a loud comment's body, a custom property's value — but
//! text that dart-sass still resolves interpolation inside. dart-sass models
//! them the same way: an `Interpolation` is a list of literal chunks and
//! embedded expressions.
//!
//! Finding the `#{…}` in a run of text is delimiter matching, not parsing. The
//! text *inside* each one is handed straight back to `oxc-css-parser`
//! ([`Evaluator::interp_expr`]), so the expression grammar still lives in
//! exactly one place — and the result is cached in the arena, so a selector
//! inside a `@for` body is parsed once however many times it is evaluated.

use oxc_css_parser::{ParserBuilder, Span, ast as ox};

use crate::error::Error;
use crate::source::SourceFile;
use super::{Evaluator, interp_checked};

/// One chunk of interpolated text: a literal run, or an expression to
/// evaluate and splice in.
///
/// `Copy`, so a resolved chunk list can live in the arena alongside the tree
/// it came from rather than being rebuilt per evaluation.
#[derive(Clone, Copy)]
pub(crate) enum Chunk<'a> {
    /// A byte range of the file's text, taken verbatim.
    Lit(Span),
    /// A parsed `#{…}` body, with the span of the whole `#{…}` construct.
    Expr {
        values: &'a ox::ComponentValues<'a>,
        span: Span,
    },
}

impl<'a> Evaluator<'a> {
    /// The source text of `span`, with every `#{…}` in it replaced by the
    /// value of its expression.
    pub(super) fn interpolate(&mut self, span: Span) -> Result<String, Error> {
        Ok(self.interpolate_bounds(span)?.0)
    }

    /// [`Self::interpolate`], additionally reporting each interpolation's
    /// (char start, char length) range in the output — used to decide whether
    /// an error's column falls inside interpolated text, which is what turns a
    /// resolved-selector error into the dual-span "error in interpolated
    /// output" diagnostic.
    pub(super) fn interpolate_bounds(
        &mut self,
        span: Span,
    ) -> Result<(String, Vec<(usize, usize)>), Error> {
        let file = self.file;
        let chunks = self.chunks(span)?;
        let mut out = String::new();
        let mut chars = 0usize;
        let mut bounds = Vec::new();
        for chunk in chunks {
            match chunk {
                Chunk::Lit(s) => {
                    let text = file.text_of(*s);
                    out.push_str(text);
                    chars += text.chars().count();
                }
                Chunk::Expr { values, span } => {
                    let value = self.eval_values(&values.values)?;
                    let text = interp_checked(&value).map_err(|e| e.or_at(file.pos(*span), 0))?;
                    let len = text.chars().count();
                    bounds.push((chars, len));
                    out.push_str(&text);
                    chars += len;
                }
            }
        }
        Ok((out, bounds))
    }

    /// Whether `span`'s text contains any interpolation at all. A selector or
    /// prelude with none can skip the whole resolve-and-reparse path.
    pub(super) fn has_interp(&self, span: Span) -> bool {
        find_interp(self.file.text_of(span)).is_some()
    }

    /// Split `span` into literal and `#{…}` chunks, parsing each
    /// interpolation body once and caching it in the arena.
    fn chunks(&mut self, span: Span) -> Result<&'a [Chunk<'a>], Error> {
        let file = self.file;
        if let Some(cached) = self.interp_cache.get(&(file.text.as_ptr() as usize, span.start, span.end)) {
            return Ok(cached);
        }
        let text = file.text_of(span);
        let base = span.start;

        let mut chunks: Vec<Chunk<'a>> = Vec::new();
        let mut i = 0usize;
        while let Some(rel) = find_interp(&text[i..]) {
            let open = i + rel;
            let body = open + 2;
            let Some(close) = match_close(text, body) else {
                // An unclosed `#{` cannot occur in text the parser accepted;
                // treating the rest as literal keeps this total rather than
                // inventing an error the parser did not raise.
                break;
            };
            if open > i {
                chunks.push(Chunk::Lit(Span {
                    start: base + i,
                    end: base + open,
                }));
            }
            chunks.push(Chunk::Expr {
                values: self.interp_expr(base + body, base + close)?,
                span: Span {
                    start: base + open,
                    end: base + close + 1,
                },
            });
            i = close + 1;
        }
        if i < text.len() {
            chunks.push(Chunk::Lit(Span {
                start: base + i,
                end: base + text.len(),
            }));
        }

        // `Chunk` is `Copy`, so the arena can own the slice outright.
        let chunks: &'a [Chunk<'a>] = self.arena.alloc_slice_copy(&chunks);
        self.interp_cache
            .insert((file.text.as_ptr() as usize, span.start, span.end), chunks);
        Ok(chunks)
    }

    /// Parse `start..end` of the file as a SassScript expression.
    ///
    /// The range is a slice of the text the outer parse already saw, so the
    /// sub-parse's spans line up with the outer ones once shifted — which is
    /// why the fragment is padded rather than copied out: parsing
    /// `"        $a + 1"` in place keeps every span pointing at the real file.
    fn interp_expr(
        &mut self,
        start: usize,
        end: usize,
    ) -> Result<&'a ox::ComponentValues<'a>, Error> {
        let file = self.file;
        // Whitespace-pad the fragment up to its real offset so the parser's
        // spans are absolute; the padding costs one arena string per distinct
        // interpolation, parsed once and cached by the caller.
        let mut padded = String::with_capacity(end);
        padded.extend(std::iter::repeat_n(' ', start));
        padded.push_str(file.text.get(start..end).unwrap_or(""));
        if padded.trim().is_empty() {
            return Err(Error::at("Expected expression.", file.pos_at(start)));
        }
        let padded: &'a str = self.arena.alloc_str(&padded);

        let mut parser = ParserBuilder::new(self.arena, padded)
            .syntax(file.syntax())
            .build();
        let values = parser
            .parse::<ox::ComponentValues<'a>>()
            .map_err(|e| file.syntax_error(&e))?;
        if let Some(first) = parser.recoverable_errors().first() {
            return Err(file.syntax_error(first));
        }
        Ok(self.arena.alloc(values))
    }
}

/// The byte index of the next `#{`, if any.
fn find_interp(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(1)).find(|&i| b[i] == b'#' && b[i + 1] == b'{')
}

/// The byte index of the `}` closing an interpolation whose body starts at
/// `from`, tracking nested braces and skipping over quoted strings — whose own
/// braces must not be counted, but whose nested interpolations must.
fn match_close(s: &str, from: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0usize;
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'"' | b'\'' => {
                let quote = b[i];
                i += 1;
                while i < b.len() && b[i] != quote {
                    if b[i] == b'\\' {
                        i += 1;
                    } else if b[i] == b'#' && b.get(i + 1) == Some(&b'{') {
                        i = match_close(s, i + 2)?;
                    }
                    i += 1;
                }
            }
            b'{' => depth += 1,
            b'}' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The source span of one `#{…}` inside a selector, recorded so an error
/// blamed on the interpolation's *output* can also point at the expression
/// that produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InterpSpan {
    /// 1-based line of the interpolated expression's first character.
    pub line: u32,
    /// 1-based column of the expression's first character (just past `#{`).
    pub col_start: u32,
    /// 1-based column of the closing `}`.
    pub col_end: u32,
    /// 0-based byte offset of the expression's first character.
    pub offset: u32,
}

impl<'a> Evaluator<'a> {
    /// The source spans of `span`'s top-level interpolations, in order — the
    /// second half of the dual-span "error in interpolated output" diagnostic.
    pub(super) fn interp_spans(&mut self, span: Span) -> Result<Vec<InterpSpan>, Error> {
        let file = self.file;
        let chunks = self.chunks(span)?;
        Ok(chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Lit(_) => None,
                Chunk::Expr { values, span } => {
                    // The chunk's span covers the whole `#{…}`; the expression
                    // starts just past `#{` and the caret ends at the `}`.
                    let start = file.pos_at(values.span.start);
                    let end = file.pos_at(span.end.saturating_sub(1));
                    Some(InterpSpan {
                        line: start.line as u32,
                        col_start: start.col as u32,
                        col_end: end.col as u32,
                        offset: start.offset as u32,
                    })
                }
            })
            .collect())
    }
}
