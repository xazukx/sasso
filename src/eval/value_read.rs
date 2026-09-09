//! Reading the parser's value nodes.
//!
//! `oxc-css-parser` hands a value back as a flat run of component values with
//! `,` and `/` kept as [`Delimiter`](ox::Delimiter) markers, and an argument
//! list the same way. Grouping that run into comma lists, slash divisions,
//! space lists and arguments is what this module does.
//!
//! Nothing here builds a tree: [`Arg`] and [`Group`] are borrowed *views* onto
//! the parser's own nodes, so evaluation always reads the parse directly.

use oxc_css_parser::token::Token;
use oxc_css_parser::{Span, ast as ox};

use crate::value::ListSep;

/// One argument of a call, as a borrowed view over the parser's nodes.
#[derive(Clone, Copy)]
pub(crate) struct Arg<'a> {
    /// The `$name` of a keyword argument, without the `$`.
    pub name: Option<&'a str>,
    /// The argument's value: the run of component values that spell it.
    pub values: &'a [ox::ComponentValue<'a>],
    /// `$list...` — expanded into positional/keyword arguments at call time.
    pub splat: bool,
    /// The whole argument's span, for blaming an error on it.
    pub span: Span,
}

impl<'a> Arg<'a> {
    /// Whether this argument is a plain positional one (no `$name:`, no `...`),
    /// which is what the CSS-calculation paths require of every argument.
    pub fn is_plain(&self) -> bool {
        self.name.is_none() && !self.splat
    }
}

/// Split a call's arguments on top-level commas.
///
/// `keep_empty_last` preserves a trailing empty argument, which only `var()`
/// wants (`var(--c,)` keeps its empty second argument); everywhere else a
/// trailing comma adds no argument.
pub(crate) fn args_of<'a>(
    args: &'a [ox::ComponentValue<'a>],
    keep_empty_last: bool,
) -> Vec<Arg<'a>> {
    let mut out = Vec::new();
    let mut groups = split_on(args, ox::DelimiterKind::Comma);
    if !keep_empty_last && groups.last().is_some_and(|g| g.is_empty()) {
        groups.pop();
    }
    for group in groups {
        let span = group_span(group, args);
        // A keyword or splat argument is the whole group; the parser gives
        // each its own node, so recognising one is a single match.
        match group {
            [ox::ComponentValue::SassKeywordArgument(k)] => out.push(Arg {
                name: Some(k.name.name.name),
                values: std::slice::from_ref(&k.value),
                splat: false,
                span: k.span,
            }),
            [ox::ComponentValue::SassArbitraryArgument(a)] => out.push(Arg {
                name: None,
                values: std::slice::from_ref(&a.value),
                splat: true,
                span: a.span,
            }),
            _ => out.push(Arg {
                name: None,
                values: group,
                splat: false,
                span,
            }),
        }
    }
    out
}

/// Read an argument list the parser already split for us.
///
/// `@include` and `@content` report their arguments as one node each with the
/// commas held separately (`SassIncludeArgs::comma_spans`), unlike a function
/// call, whose arguments arrive as one flat run with `Delimiter::Comma` in it.
pub(crate) fn args_presplit<'a>(args: &'a [ox::ComponentValue<'a>]) -> Vec<Arg<'a>> {
    args.iter().map(arg_of).collect()
}

/// One already-separated argument.
fn arg_of<'a>(value: &'a ox::ComponentValue<'a>) -> Arg<'a> {
    match value {
        ox::ComponentValue::SassKeywordArgument(k) => Arg {
            name: Some(k.name.name.name),
            values: std::slice::from_ref(&k.value),
            splat: false,
            span: k.span,
        },
        ox::ComponentValue::SassArbitraryArgument(a) => Arg {
            name: None,
            values: std::slice::from_ref(&a.value),
            splat: true,
            span: a.span,
        },
        other => Arg {
            name: None,
            values: std::slice::from_ref(other),
            splat: false,
            span: *other.span(),
        },
    }
}

/// Split a value run on top-level commas.
///
/// A trailing comma adds no element (`a, b,` is a two-element list), which is
/// the only place the parser's flat run needs interpreting.
pub(crate) fn split_commas<'a>(
    values: &'a [ox::ComponentValue<'a>],
) -> Vec<&'a [ox::ComponentValue<'a>]> {
    let mut out = split_on(values, ox::DelimiterKind::Comma);
    if out.len() > 1 && out.last().is_some_and(|g| g.is_empty()) {
        out.pop();
    }
    out
}

/// Split a run on a top-level delimiter, keeping empty groups.
fn split_on<'a>(
    values: &'a [ox::ComponentValue<'a>],
    kind: ox::DelimiterKind,
) -> Vec<&'a [ox::ComponentValue<'a>]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, v) in values.iter().enumerate() {
        if matches!(v, ox::ComponentValue::Delimiter(d) if same_kind(&d.kind, &kind)) {
            out.push(&values[start..i]);
            start = i + 1;
        }
    }
    out.push(&values[start..]);
    out
}

/// `DelimiterKind` carries no `PartialEq`, so compare by discriminant.
fn same_kind(a: &ox::DelimiterKind, b: &ox::DelimiterKind) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// Whether a component value is a top-level `/`.
pub(crate) fn is_solidus(v: &ox::ComponentValue<'_>) -> bool {
    matches!(v, ox::ComponentValue::Delimiter(d) if matches!(d.kind, ox::DelimiterKind::Solidus))
}

/// Whether a component value is a top-level `;`.
pub(crate) fn is_semicolon(v: &ox::ComponentValue<'_>) -> bool {
    matches!(v, ox::ComponentValue::Delimiter(d) if matches!(d.kind, ox::DelimiterKind::Semicolon))
}

/// Split a run on top-level `;`, which only the modern CSS `if()` uses to
/// separate its clauses.
pub(crate) fn split_clauses<'a>(
    values: &'a [ox::ComponentValue<'a>],
) -> Vec<&'a [ox::ComponentValue<'a>]> {
    split_on(values, ox::DelimiterKind::Semicolon)
}

/// The span covering a run of values, falling back to the enclosing run when
/// the group is empty (a trailing or omitted argument has no text of its own).
pub(crate) fn group_span<'a>(
    group: &'a [ox::ComponentValue<'a>],
    outer: &'a [ox::ComponentValue<'a>],
) -> Span {
    match (group.first(), group.last()) {
        (Some(first), Some(last)) => Span {
            start: first.span().start,
            end: last.span().end,
        },
        _ => outer.last().map_or(Span { start: 0, end: 0 }, |v| Span {
            start: v.span().end,
            end: v.span().end,
        }),
    }
}

/// The span of a value run.
pub(crate) fn values_span<'a>(values: &'a [ox::ComponentValue<'a>]) -> Span {
    group_span(values, values)
}

/// The declared parameters of a callable. A `@mixin` written with no `(…)`
/// has no parameter node at all, which reads as an empty list.
pub(crate) fn params_of<'a>(
    params: Option<&'a ox::SassParameters<'a>>,
) -> &'a [ox::SassParameter<'a>] {
    params.map_or(&[], |p| &p.params)
}

/// The name of a callable's `$rest...` parameter, if it declares one.
pub(crate) fn rest_of<'a>(params: Option<&'a ox::SassParameters<'a>>) -> Option<&'a str> {
    params?.arbitrary_param.as_ref().map(|a| a.name.name.name)
}

/// Whether a component value is a bare `:` token.
///
/// The modern CSS `if()` separates a clause's condition from its value with
/// one, and the parser keeps it as a raw token because `<condition>: <value>`
/// is not an argument shape the value grammar owns.
pub(crate) fn is_colon(v: &ox::ComponentValue<'_>) -> bool {
    matches!(v, ox::ComponentValue::TokenWithSpan(t) if matches!(t.token, Token::Colon(..)))
}

/// Split a modern `if()` clause into its condition and value around the first
/// top-level `:`. `None` when the run has none, which means the arguments are
/// not in clause form at all.
pub(crate) fn split_clause<'a>(
    clause: &'a [ox::ComponentValue<'a>],
) -> Option<(&'a [ox::ComponentValue<'a>], &'a [ox::ComponentValue<'a>])> {
    let i = clause.iter().position(is_colon)?;
    Some((&clause[..i], &clause[i + 1..]))
}
