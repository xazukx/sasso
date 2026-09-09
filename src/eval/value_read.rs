//! Reading the parser's value nodes.
//!
//! `oxc-css-parser` hands a value back as a flat run of component values with
//! `,` and `/` kept as [`Delimiter`](ox::Delimiter) markers, and an argument
//! list the same way. Grouping that run into comma lists, slash divisions,
//! space lists and arguments is what this module does.
//!
//! Nothing here builds a tree: [`Arg`] and [`Group`] are borrowed *views* onto
//! the parser's own nodes, so evaluation always reads the parse directly.

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

/// A value run grouped into its top-level elements and the separator that
/// joined them.
pub(crate) struct Group<'a> {
    /// Each element, still as the run of component values that spells it.
    pub items: Vec<&'a [ox::ComponentValue<'a>]>,
    /// The separator joining them. [`ListSep::Undecided`] when there is a
    /// single element and nothing joined it.
    pub sep: ListSep,
}

/// Group a value run into its top-level elements: commas win over spaces, and
/// a run with neither is a single element.
///
/// `/` is *not* a separator here — it is the division operator, handled while
/// evaluating an element (see `Evaluator::eval_slash_chain`), because whether
/// it divides or keeps its slash spelling depends on the operands.
pub(crate) fn group<'a>(values: &'a [ox::ComponentValue<'a>]) -> Group<'a> {
    let commas = split_on(values, ox::DelimiterKind::Comma);
    if commas.len() > 1 {
        let mut items = commas;
        // A trailing comma does not add an element: `a, b,` is a two-element
        // list.
        if items.last().is_some_and(|g| g.is_empty()) {
            items.pop();
        }
        return Group {
            items,
            sep: ListSep::Comma,
        };
    }

    let single = commas.into_iter().next().unwrap_or(&[]);
    if single.is_empty() {
        return Group {
            items: Vec::new(),
            sep: ListSep::Undecided,
        };
    }
    // Everything adjacent is a space list — unless a `/` is in play, in which
    // case the whole run is one division expression.
    if single.len() == 1 || single.iter().any(is_solidus) {
        return Group {
            items: vec![single],
            sep: ListSep::Undecided,
        };
    }
    Group {
        items: single.iter().map(std::slice::from_ref).collect(),
        sep: ListSep::Space,
    }
}

/// Split a run on a top-level delimiter, keeping empty groups.
fn split_on<'a>(
    values: &'a [ox::ComponentValue<'a>],
    kind: ox::DelimiterKind,
) -> Vec<&'a [ox::ComponentValue<'a>]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, v) in values.iter().enumerate() {
        if matches!(v, ox::ComponentValue::Delimiter(d) if d.kind == kind) {
            out.push(&values[start..i]);
            start = i + 1;
        }
    }
    out.push(&values[start..]);
    out
}

/// Whether a component value is a top-level `/`.
pub(crate) fn is_solidus(v: &ox::ComponentValue<'_>) -> bool {
    matches!(v, ox::ComponentValue::Delimiter(d) if d.kind == ox::DelimiterKind::Solidus)
}

/// Whether a component value is a top-level `;`.
pub(crate) fn is_semicolon(v: &ox::ComponentValue<'_>) -> bool {
    matches!(v, ox::ComponentValue::Delimiter(d) if d.kind == ox::DelimiterKind::Semicolon)
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
