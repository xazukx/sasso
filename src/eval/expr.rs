use super::value_read::{Arg, args_of, is_solidus, split_commas};
use super::*;

impl<'a> Evaluator<'a> {
    // ---- interpolation & the parent selector -------------------------

    /// The value of `&` in value position: the current resolved selector list
    /// as a comma-separated Sass list where each item is one complex selector
    /// (a space-separated list of compound-selector strings). At the document
    /// root (no enclosing style rule) this is `null`. This matches dart-sass,
    /// where `&` is always a comma list even for a single selector.
    pub(super) fn parent_selector_value(&self) -> Value {
        let Some(selectors) = &self.current_selector else {
            return Value::Null;
        };
        if selectors.is_empty() {
            return Value::Null;
        }
        let items: Vec<Value> = selectors
            .iter()
            .map(|complex| {
                // Every complex selector is a SPACE LIST of compounds (dart:
                // `meta.type-of(list.nth(&, 1))` is `list` even for `.foo`).
                let compounds: Vec<Value> = complex
                    .split_whitespace()
                    .map(|c| {
                        Value::Str(SassStr {
                            text: c.to_string().into(),
                            quoted: false,
                        })
                    })
                    .collect();
                Value::List(List {
                    items: compounds.into(),
                    sep: ListSep::Space,
                    bracketed: false,
                    keywords: None,
                })
            })
            .collect();
        Value::List(List {
            items: items.into(),
            sep: ListSep::Comma,
            bracketed: false,
            keywords: None,
        })
    }

    // ---- value runs --------------------------------------------------

    /// Evaluate a run of component values — a declaration's value, an
    /// argument, an interpolation body — as one SassScript value.
    ///
    /// The parser keeps `,` and `/` as delimiters in the run rather than
    /// building list nodes, so the grouping happens here: commas make a comma
    /// list, adjacency makes a space list, and `/` divides (or keeps its
    /// deprecated slash spelling, see [`Self::eval_slash_chain`]).
    pub(super) fn eval_values(
        &mut self,
        values: &'a [ox::ComponentValue<'a>],
    ) -> Result<Value, Error> {
        let groups = split_commas(values);
        if groups.len() > 1 {
            let mut vals = Vec::with_capacity(groups.len());
            for g in &groups {
                vals.push(self.eval_element_run(g)?);
            }
            return Ok(Value::List(List {
                items: vals.into(),
                sep: ListSep::Comma,
                bracketed: false,
                keywords: None,
            }));
        }
        self.eval_element_run(groups.first().copied().unwrap_or(&[]))
    }

    /// Evaluate one comma-group: a run of adjacent values, with any `/` folded
    /// into its immediate neighbours.
    ///
    /// `/` binds tighter than juxtaposition, so `1 2 3 / 0.5` is the
    /// three-element space list `1 2 (3/0.5)` — which is how `rgb(1 2 3 / 0.5)`
    /// reaches the builtin as a list whose last element carries the slash.
    fn eval_element_run(
        &mut self,
        values: &'a [ox::ComponentValue<'a>],
    ) -> Result<Value, Error> {
        if values.is_empty() {
            return Ok(Value::List(List {
                items: Vec::new().into(),
                sep: ListSep::Undecided,
                bracketed: false,
                keywords: None,
            }));
        }
        let mut elems: Vec<Value> = Vec::new();
        // Whether each element is still eligible to keep the deprecated slash
        // spelling — true only for number literals and chains of them.
        let mut slashy: Vec<bool> = Vec::new();
        let mut i = 0usize;
        while i < values.len() {
            if is_solidus(&values[i]) {
                let op = *values[i].span();
                i += 1;
                let Some(rhs_node) = values.get(i) else {
                    // A trailing `/` with nothing after it is text.
                    elems.push(Value::Str(SassStr {
                        text: "/".to_string().into(),
                        quoted: false,
                    }));
                    slashy.push(false);
                    break;
                };
                let rhs = self.eval_value(rhs_node)?;
                let rhs_ok = is_slash_operand_node(rhs_node);
                let (lhs, lhs_ok) = match elems.pop() {
                    Some(v) => (v, slashy.pop().unwrap_or(false)),
                    // A leading `/` divides nothing; dart reads it as text.
                    None => (
                        Value::Str(SassStr {
                            text: String::new().into(),
                            quoted: false,
                        }),
                        false,
                    ),
                };
                // Inside a calculation `/` is always real division.
                let slash = self.calc_depth == 0 && lhs_ok && rhs_ok;
                elems.push(eval_div(lhs, rhs, slash, self.file.pos(op))?);
                slashy.push(slash);
                i += 1;
                continue;
            }
            elems.push(self.eval_value(&values[i])?);
            slashy.push(is_slash_operand_node(&values[i]));
            i += 1;
        }
        if elems.len() == 1 {
            return Ok(elems.pop().expect("len checked"));
        }
        Ok(Value::List(List {
            items: elems.into(),
            sep: ListSep::Space,
            bracketed: false,
            keywords: None,
        }))
    }

    // ---- single values -----------------------------------------------

    /// Evaluate one component value.
    ///
    /// Give the error this node's own position if a helper deeper down raised
    /// it without one, then finalize it into a rendered diagnostic block here,
    /// where `current_url`/`current_source`/`call_stack` still describe the
    /// file and call context it was raised in. Since this runs at every level
    /// of the value tree, the innermost node that HAS a span is the one that
    /// gets blamed — which is where dart-sass blames it too.
    pub(super) fn eval_value(
        &mut self,
        value: &'a ox::ComponentValue<'a>,
    ) -> Result<Value, Error> {
        match self.eval_value_inner(value) {
            Ok(v) => Ok(v),
            Err(e) => {
                let span = *value.span();
                let e = e.or_at(self.file.pos(span), span.end.saturating_sub(span.start));
                Err(self.finalize_error(e))
            }
        }
    }

    fn eval_value_inner(
        &mut self,
        value: &'a ox::ComponentValue<'a>,
    ) -> Result<Value, Error> {
        use ox::ComponentValue as CV;
        match value {
            CV::Number(n) => Ok(Value::Number(Number::unitless(number(n)))),
            CV::Percentage(p) => Ok(Value::Number(Number::with_unit(
                number(&p.value),
                "%".to_string(),
            ))),
            CV::Dimension(d) => Ok(Value::Number(Number::with_unit(
                number(&d.value),
                d.unit.name.to_string(),
            ))),
            CV::HexColor(h) => Ok(match crate::value::Color::from_hex(h.value) {
                Some(c) => Value::Color(c),
                // The parser accepts hash tokens that are not colors
                // (`#abcde`); dart-sass keeps those as identifiers.
                None => Value::Str(SassStr {
                    text: format!("#{}", h.raw).into(),
                    quoted: false,
                }),
            }),
            CV::InterpolableIdent(i) => self.eval_ident(i),
            CV::InterpolableStr(s) => {
                // A string expression resolves its interpolation in a context
                // where the `@supports`-declaration no-simplify flag is OFF,
                // so `(a: #{calc(1 + 2)})` -> `(a: 3)`, matching dart-sass
                // `visitStringExpression`.
                let saved = std::mem::replace(&mut self.in_supports_declaration, false);
                let text = self.eval_interp_str(s);
                self.in_supports_declaration = saved;
                Ok(Value::Str(SassStr {
                    text: text?.into(),
                    quoted: true,
                }))
            }
            CV::SassVariable(v) => self.eval_variable(v.name.name, self.file.pos(v.span)),
            CV::SassQualifiedName(q) => match &q.member {
                ox::SassModuleMemberName::Variable(v) => self
                    // The caret covers the whole `ns.$name` reference, exactly
                    // as dart-sass blames `MemberExpression`'s span.
                    .eval_module_var(q.module.name, v.name.name, self.file.pos(q.span))
                    .map_err(|e| {
                        e.with_length(q.module.name.len() + 2 + v.name.name.len())
                    }),
                // A bare `ns.member` with no `$` is only a value when it is a
                // call, which the parser reports as a `Function` instead.
                ox::SassModuleMemberName::Ident(_) => self.eval_text_value(q.span),
            },
            CV::SassParentSelector(n) => match &n.suffix {
                None => Ok(self.parent_selector_value()),
                // `&foo` in value position is the parent selector glued to a
                // suffix, which dart-sass reads as a plain identifier run.
                Some(_) => self.eval_text_value(n.span),
            },
            // Parentheses force the deprecated slash to perform real
            // division: `(1/2)` is `0.5`, not the slash value `1/2`.
            CV::SassParenthesizedExpression(p) => {
                Ok(self.eval_value(&p.expr)?.without_slash())
            }
            CV::SassUnaryExpression(u) => self.eval_unary(u),
            CV::SassBinaryExpression(b) => self.eval_binary_expr(b),
            CV::SassList(l) => self.eval_sass_list(l, false),
            CV::BracketBlock(b) => {
                // `[a, b]` and `[a b]` are bracketed lists; the separator is
                // whatever joined them, and a single element decides nothing.
                let groups = split_commas(&b.value);
                let (items, sep) = if groups.len() > 1 {
                    let mut vals = Vec::with_capacity(groups.len());
                    for g in &groups {
                        vals.push(self.eval_element_run(g)?);
                    }
                    (vals, ListSep::Comma)
                } else {
                    match self.eval_element_run(groups.first().copied().unwrap_or(&[]))? {
                        Value::List(l) if !l.bracketed => {
                            (l.items.to_vec(), l.sep)
                        }
                        one => (vec![one], ListSep::Undecided),
                    }
                };
                Ok(Value::List(List {
                    items: items.into(),
                    sep,
                    bracketed: true,
                    keywords: None,
                }))
            }
            CV::SassMap(m) => self.eval_map(m),
            CV::Function(f) => self.eval_function(f),
            CV::Url(u) => self.eval_url(u),
            CV::Calc(c) => self.eval_calc_node_value(c),
            CV::Ratio(r) => eval_div(
                Value::Number(Number::unitless(number(&r.numerator))),
                Value::Number(Number::unitless(number(&r.denominator))),
                true,
                self.file.pos(r.solidus_span),
            ),
            // A keyword or splat argument outside an argument list, and every
            // remaining node kind — raw tokens, unicode ranges, an id selector
            // used as a value (`nav-up: #a`) — read as their own text.
            // dart-sass normalizes the annotation's spelling: `! Important`
            // and `!IMPORTANT` both serialize as `!important`.
            CV::ImportantAnnotation(_) => Ok(Value::Str(SassStr {
                text: "!important".to_string().into(),
                quoted: false,
            })),
            CV::SassKeywordArgument(k) => self.eval_value(&k.value),
            CV::SassArbitraryArgument(a) => self.eval_value(&a.value),
            other => self.eval_text_value(*other.span()),
        }
    }

    /// A node with no SassScript meaning of its own: its source text, with
    /// interpolation resolved, as an unquoted string.
    fn eval_text_value(&mut self, span: Span) -> Result<Value, Error> {
        let saved = std::mem::replace(&mut self.in_supports_declaration, false);
        let text = self.interpolate(span);
        self.in_supports_declaration = saved;
        Ok(Value::Str(SassStr {
            text: text?.into(),
            quoted: false,
        }))
    }

    /// An identifier in value position: the SassScript keywords, then a named
    /// color, then a plain (possibly interpolated) identifier.
    fn eval_ident(&mut self, i: &'a ox::InterpolableIdent<'a>) -> Result<Value, Error> {
        if let ox::InterpolableIdent::Literal(id) = i {
            // In plain CSS `true`/`false`/`null` have no special meaning; they
            // stay plain identifiers (dart-sass `CssParser`).
            if !self.in_plain_css {
                match id.name {
                    "true" => return Ok(Value::Bool(true)),
                    "false" => return Ok(Value::Bool(false)),
                    "null" => return Ok(Value::Null),
                    _ => {}
                }
            }
            if let Some(color) = crate::value::named_color(id.name) {
                return Ok(Value::Color(color));
            }
            return Ok(Value::Str(SassStr {
                text: id.raw.to_string().into(),
                quoted: false,
            }));
        }
        // An interpolated identifier resolves its interpolation with the
        // `@supports` no-simplify flag off, like a string expression.
        let saved = std::mem::replace(&mut self.in_supports_declaration, false);
        let text = self.eval_interp_ident(i);
        self.in_supports_declaration = saved;
        let text = text?;
        // dart-sass re-reads the resolved text: an interpolation that produces
        // a keyword or a color name still yields a plain unquoted string, so
        // only the literal arm above consults them.
        Ok(Value::Str(SassStr {
            text: text.into(),
            quoted: false,
        }))
    }

    /// An interpolable identifier's text, with `#{…}` resolved.
    pub(super) fn eval_interp_ident(
        &mut self,
        i: &'a ox::InterpolableIdent<'a>,
    ) -> Result<String, Error> {
        match i {
            // The source spelling is kept, not the decoded one: an escape in
            // an identifier is part of how it serializes.
            ox::InterpolableIdent::Literal(id) => Ok(id.raw.to_string()),
            ox::InterpolableIdent::SassInterpolated(si) => {
                let mut out = String::new();
                for e in &si.elements {
                    match e {
                        ox::SassInterpolatedIdentElement::Static(p) => out.push_str(p.raw),
                        ox::SassInterpolatedIdentElement::Expression(cv) => {
                            let v = self.eval_value(cv)?;
                            out.push_str(&interp_checked(&v)?);
                        }
                    }
                }
                Ok(out)
            }
            other => self.interpolate(*other.span()),
        }
    }

    /// An interpolable string's *contents*, with `#{…}` resolved and the
    /// surrounding quotes removed.
    pub(super) fn eval_interp_str(
        &mut self,
        s: &'a ox::InterpolableStr<'a>,
    ) -> Result<String, Error> {
        match s {
            ox::InterpolableStr::Literal(lit) => Ok(unquote_raw(lit.raw).to_string()),
            ox::InterpolableStr::SassInterpolated(si) => {
                let mut out = String::new();
                for e in &si.elements {
                    match e {
                        ox::SassInterpolatedStrElement::Static(p) => {
                            out.push_str(unquote_raw(p.raw))
                        }
                        ox::SassInterpolatedStrElement::Expression(cv) => {
                            let v = self.eval_value(cv)?;
                            out.push_str(&interp_checked(&v)?);
                        }
                    }
                }
                Ok(out)
            }
            other => self.interpolate(*other.span()),
        }
    }

    /// Read a variable, honouring the unprefixed `@use … as *` namespaces.
    ///
    /// Reading a variable drops a bare slash-division's spelling (dart-sass
    /// `withoutSlash`): `$x: 1/2; a {b: $x}` is `0.5`. Slashes nested inside a
    /// stored list are preserved.
    pub(super) fn eval_variable(&mut self, name: &str, pos: Pos) -> Result<Value, Error> {
        if let Some(v) = self.lookup(name) {
            // `lookup` already returns an owned clone out of the scope;
            // `without_slash` consumes it, so a second clone is wasted.
            return Ok(v.without_slash());
        }
        // A user module variable exposed unprefixed via `@use … as *`.
        let star_hits: Vec<Value> = if is_private_member(name) {
            Vec::new()
        } else {
            self.star_user_modules
                .iter()
                .filter_map(|m| m.var(name))
                .collect()
        };
        if star_hits.len() > 1 {
            return Err(Error::at(
                "This variable is available from multiple global modules.",
                pos,
            ));
        }
        if let Some(v) = star_hits.into_iter().next() {
            return Ok(v.without_slash());
        }
        // A built-in module variable exposed unprefixed via
        // `@use "sass:…" as *` (e.g. `$pi` from `sass:math`).
        for m in &self.star_modules {
            if let Ok(v) = crate::builtins::module_var(m, name, pos) {
                return Ok(v);
            }
        }
        // The caret covers `$name` (the `$` plus the identifier).
        Err(Error::at("Undefined variable.", pos).with_length(1 + name.len()))
    }

    fn eval_unary(&mut self, u: &'a ox::SassUnaryExpression<'a>) -> Result<Value, Error> {
        let v = self.eval_value(&u.expr)?.without_slash();
        match u.op.kind {
            // Unary minus negates a number; on any other operand dart-sass
            // produces an unquoted `-<value>` string (`- red` -> `-red`,
            // `- "q"` -> `-"q"`). A calculation that could not reduce to a
            // number has no negation operator, so dart-sass rejects it
            // ("Undefined operation \"-calc(…)\".").
            ox::SassUnaryOperatorKind::Minus => match v {
                Value::Number(n) => Ok(Value::Number(n.copy_units(-n.value))),
                Value::Calc(_) => Err(Error::unpositioned(format!(
                    "Undefined operation \"-{}\".",
                    v.to_css(false)
                ))),
                // A map or empty list cannot be serialized into the unquoted
                // `-<value>` string dart-sass produces, so it errors before
                // the join (`-(a: 1)`, `-()`).
                other => match css_value_error_msg(&other) {
                    Some(msg) => Err(Error::unpositioned(msg)),
                    None => Ok(Value::Str(SassStr {
                        text: format!("-{}", other.to_css(false)).into(),
                        quoted: false,
                    })),
                },
            },
            // Unary plus is numeric identity; on any other operand it prepends
            // `+` as an unquoted string (`+foo` -> `+foo`). A residual
            // calculation has no unary-plus operator and is rejected the same
            // way.
            ox::SassUnaryOperatorKind::Plus => match v {
                Value::Number(_) => Ok(v),
                Value::Calc(_) => Err(Error::unpositioned(format!(
                    "Undefined operation \"+{}\".",
                    v.to_css(false)
                ))),
                other => match css_value_error_msg(&other) {
                    Some(msg) => Err(Error::unpositioned(msg)),
                    None => Ok(Value::Str(SassStr {
                        text: format!("+{}", other.to_css(false)).into(),
                        quoted: false,
                    })),
                },
            },
            ox::SassUnaryOperatorKind::Not => Ok(Value::Bool(!v.is_truthy())),
        }
    }

    fn eval_binary_expr(
        &mut self,
        b: &'a ox::SassBinaryExpression<'a>,
    ) -> Result<Value, Error> {
        use ox::SassBinaryOperatorKind as K;
        let pos = self.file.pos(b.op.span);

        // `and`/`or` short-circuit and yield a value, so the right operand is
        // only evaluated when needed.
        if matches!(b.op.kind, K::And | K::Or) {
            let l = self.eval_value(&b.left)?;
            let and = matches!(b.op.kind, K::And);
            return match (and, l.is_truthy()) {
                (true, true) | (false, false) => self.eval_value(&b.right),
                _ => Ok(l),
            };
        }

        if matches!(b.op.kind, K::Division) {
            // `/` between number literals keeps the deprecated slash
            // spelling; inside a calculation it is always real division.
            let slash = self.calc_depth == 0
                && is_slash_operand_node(&b.left)
                && is_slash_operand_node(&b.right);
            let l = self.eval_value(&b.left)?;
            let r = self.eval_value(&b.right)?;
            return eval_div(l, r, slash, pos);
        }

        let op = match b.op.kind {
            K::Multiply => BinOp::Mul,
            K::Modulo => BinOp::Mod,
            K::Plus => BinOp::Add,
            K::Minus => BinOp::Sub,
            K::GreaterThan => BinOp::Gt,
            K::GreaterThanOrEqual => BinOp::Ge,
            K::LessThan => BinOp::Lt,
            K::LessThanOrEqual => BinOp::Le,
            K::EqualsEquals => BinOp::Eq,
            K::ExclamationEquals => BinOp::Neq,
            K::And | K::Or | K::Division => unreachable!("handled above"),
        };
        let l = self.eval_value(&b.left)?;
        let r = self.eval_value(&b.right)?;
        eval_binary(op, l.without_slash(), r.without_slash(), pos)
    }

    fn eval_sass_list(
        &mut self,
        l: &'a ox::SassList<'a>,
        bracketed: bool,
    ) -> Result<Value, Error> {
        let mut vals = Vec::with_capacity(l.elements.len());
        for e in &l.elements {
            vals.push(self.eval_value(e)?);
        }
        let sep = if l.comma_spans.is_some() {
            ListSep::Comma
        } else if vals.len() > 1 {
            ListSep::Space
        } else {
            ListSep::Undecided
        };
        Ok(Value::List(List {
            items: vals.into(),
            sep,
            bracketed,
            keywords: None,
        }))
    }

    fn eval_map(&mut self, m: &'a ox::SassMap<'a>) -> Result<Value, Error> {
        let mut map = Map {
            entries: std::rc::Rc::new(Vec::new()),
        };
        for item in &m.items {
            let key = self.eval_value(&item.key)?.without_slash();
            let val = self.eval_value(&item.value)?;
            // A duplicate literal key is an error in dart-sass, blamed on the
            // SECOND key's own span.
            if map.get(&key).is_some() {
                return Err(Error::at("Duplicate key.", self.file.pos(*item.key.span())));
            }
            map.insert(key, val);
        }
        Ok(Value::Map(map))
    }

    /// A `url()` the parser read as a URL token: its contents are text, so the
    /// call is preserved verbatim with only interpolation resolved. A `url()`
    /// whose contents are SassScript (`url($a)`) is not a URL token — the
    /// parser reports it as an ordinary function instead.
    fn eval_url(&mut self, u: &'a ox::Url<'a>) -> Result<Value, Error> {
        let inner = match &u.value {
            None => String::new(),
            Some(ox::UrlValue::Raw(raw)) => raw.raw.to_string(),
            Some(ox::UrlValue::Str(s)) => self.interpolate(*s.span())?,
            Some(ox::UrlValue::SassInterpolated(si)) => self.interpolate(si.span)?,
            Some(other) => self.interpolate(*other.span())?,
        };
        let mut modifiers = String::new();
        for m in &u.modifiers {
            modifiers.push(' ');
            modifiers.push_str(&self.interpolate(*m.span())?);
        }
        // dart-sass emits a plain URL under the bare lower-cased `url(`; a
        // vendor prefix is dropped.
        Ok(Value::Str(SassStr {
            text: format!("url({inner}{modifiers})").into(),
            quoted: false,
        }))
    }

    // ---- calls -------------------------------------------------------

    /// The name a call dispatches on, and the module namespace it is qualified
    /// with. `None` means the name itself is interpolated, which makes the
    /// call plain CSS.
    fn call_name(
        &mut self,
        name: &'a ox::FunctionName<'a>,
    ) -> Result<(Option<String>, Option<&'a str>), Error> {
        match name {
            ox::FunctionName::Ident(ox::InterpolableIdent::Literal(id)) => {
                Ok((Some(id.name.to_string()), None))
            }
            ox::FunctionName::Ident(i) => {
                // An interpolated name resolves at eval time and never
                // dispatches to a real function.
                let _ = self.eval_interp_ident(i)?;
                Ok((None, None))
            }
            ox::FunctionName::SassQualifiedName(q) => {
                let member = match &q.member {
                    ox::SassModuleMemberName::Ident(id) => id.name,
                    ox::SassModuleMemberName::Variable(v) => v.name.name,
                };
                Ok((Some(member.to_string()), Some(q.module.name)))
            }
            other => Ok((Some(self.interpolate(*other.span())?), None)),
        }
    }

    fn eval_function(&mut self, f: &'a ox::Function<'a>) -> Result<Value, Error> {
        let pos = self.file.pos(f.span);
        let length = f.span.end.saturating_sub(f.span.start);

        // An interpolated-name call is a *plain CSS* function (dart-sass
        // `PlainCssCallable`): the resolved name is never dispatched to a
        // built-in or user function; the arguments evaluate and the call
        // serializes verbatim. Keyword arguments are rejected.
        let (name, module) = self.call_name(&f.name)?;
        let Some(name) = name else {
            let fname = match &f.name {
                ox::FunctionName::Ident(i) => self.eval_interp_ident(i)?,
                other => self.interpolate(*other.span())?,
            };
            let args = args_of(&f.args, false);
            if args.iter().any(|a| a.name.is_some()) {
                return Err(Error::at(
                    "Plain CSS functions don't support keyword arguments.",
                    pos,
                ));
            }
            let mut parts: Vec<String> = Vec::with_capacity(args.len());
            for a in &args {
                let v = self.eval_values(a.values)?;
                parts.push(v.to_css(self.compressed()));
            }
            return Ok(Value::Str(SassStr {
                text: format!("{fname}({})", parts.join(", ")).into(),
                quoted: false,
            }));
        };

        // A namespaced call `ns.fn(...)` resolves only against the module
        // bound to `ns`.
        if let Some(ns) = module {
            let args = args_of(&f.args, false);
            return self.eval_module_call(ns, &name, &args, pos, length);
        }

        // A plain-CSS function call is preserved verbatim, but a Sass-only
        // global function (one with no plain-CSS meaning, e.g. `index`) is
        // rejected.
        if self.in_plain_css && is_sass_only_function(&name) {
            return Err(Error::at("This function isn't allowed in plain CSS.", pos));
        }

        // `calc()` interiors are real arithmetic, simplified at evaluation
        // time. The name is matched case-insensitively (`CaLc(1px)` -> `1px`);
        // a vendor-prefixed `-webkit-calc(…)` does not match and stays a
        // verbatim special function. An EMPTY `calc()` is not a calculation:
        // it stays a regular call so a user `@function calc()` can be invoked.
        if name.eq_ignore_ascii_case("calc") && !f.args.is_empty() {
            return self.eval_calc_call(f, pos);
        }

        // Special CSS functions (`element()`, `expression()`, and a
        // vendor-prefixed `calc()`; `type()` unprefixed) preserve their
        // arguments verbatim — they may contain `%`, `@`, `=`, IE-hack syntax
        // and other non-SassScript characters. Only `#{...}` interpolation is
        // resolved; the function name is lower-cased.
        if let Some(canonical) = special_function_name(&name) {
            let args = self.interpolate(args_span(f))?;
            return Ok(Value::Str(SassStr {
                text: format!("{canonical}({args})").into(),
                quoted: false,
            }));
        }

        // `var()` and `env()` are plain CSS functions whose arguments are
        // ordinary SassScript: dart-sass evaluates them, normalises
        // surrounding whitespace, and expands a `var($args...)` splat. They
        // route through the regular call path (neither is a Sass builtin, so
        // the result is preserved verbatim). The single exception is `var`'s
        // `allowEmptySecondArg`: `var(--c,)` keeps a trailing empty argument.
        let lower = name.to_ascii_lowercase();
        if lower == "var" || lower == "env" {
            let args = args_of(&f.args, lower == "var");
            return self.eval_plain_call(&name, &args, pos);
        }

        // The modern CSS `if()` uses `:`/`;` clause syntax and `css()`/
        // `sass()` wrappers, which the comma-argument grouping cannot
        // represent. Try the modern grammar first; a run with no top-level
        // `;` and no `:` clause falls back to the legacy `if($c, $t, $f)`.
        if name == "if" {
            if let Some(v) = self.try_eval_modern_if(&f.args)? {
                return Ok(v);
            }
        }

        let args = args_of(&f.args, false);
        self.eval_call(&name, &args, pos, length)
    }

    /// A `calc()` call: its interior is a calculation.
    fn eval_calc_call(&mut self, f: &'a ox::Function<'a>, pos: Pos) -> Result<Value, Error> {
        // dart resolves user functions BEFORE calculation semantics: a
        // user-defined `@function calc(...)` shadows the CSS calculation
        // (issue_1706), receiving the argument evaluated as an ordinary
        // expression. Vendor-prefixed `-x-calc(` stays a parse-time special
        // function and can't be overridden.
        if !self.in_plain_css {
            if let Some(callable) = self.lookup_function("calc") {
                let arg = self.eval_values(&f.args)?.without_slash();
                let func = crate::value::SassFunction {
                    name: "calc".to_string(),
                    css: false,
                    user: Some(self.capture_fn(&callable)),
                };
                return self
                    .invoke_function_ref(&func, vec![arg], Vec::new(), Pos::NONE)
                    .map(Value::without_slash);
            }
        }
        let node = self.eval_calc_values(&f.args)?;
        // Inside a `@supports` declaration the calculation is kept
        // unsimplified: the `calc()` wrapper is always preserved (even around
        // a single number), matching dart-sass `simplify: false`.
        if self.in_supports_declaration {
            return Ok(Value::Calc(node));
        }
        let _ = pos;
        match node {
            // A calculation that reduces to a single number unwraps to it —
            // including a non-finite result (dart-sass:
            // `meta.type-of(calc(NaN)) == number`); the Number's own
            // serialization restores the `calc(NaN)`/`calc(infinity * 1px)`
            // spelling.
            CalcNode::Number(n) => Ok(Value::Number(n)),
            // `calc()` wrapping a single already-complete calculation
            // (`calc(min(1%, 2px))`, `calc(clamp(…))`, etc.) is redundant:
            // dart-sass drops the outer `calc()` and emits the inner
            // calculation directly. (A non-calculation leaf such as
            // `calc(var(--x))` keeps its wrapper.)
            CalcNode::Str(s) if is_complete_calculation(&s) => Ok(Value::Str(SassStr {
                text: s.into(),
                quoted: false,
            })),
            other => Ok(Value::Calc(other)),
        }
    }

    /// A `calc()`-interior operation node, evaluated as a calculation.
    fn eval_calc_node_value(&mut self, c: &'a ox::Calc<'a>) -> Result<Value, Error> {
        let node = self.eval_calc_op(c)?;
        Ok(match node {
            CalcNode::Number(n) => Value::Number(n),
            other => Value::Calc(other),
        })
    }

    /// A call whose result is preserved verbatim: the arguments evaluate but
    /// no function is looked up.
    fn eval_plain_call(
        &mut self,
        name: &str,
        args: &[Arg<'a>],
        pos: Pos,
    ) -> Result<Value, Error> {
        let _ = pos;
        let mut parts: Vec<String> = Vec::with_capacity(args.len());
        for a in args {
            if a.values.is_empty() {
                // `var(--c,)`'s explicit empty second argument.
                parts.push(String::new());
                continue;
            }
            parts.push(self.eval_values(a.values)?.to_css(self.compressed()));
        }
        Ok(Value::Str(SassStr {
            text: format!("{name}({})", parts.join(", ")).into(),
            quoted: false,
        }))
    }
}

/// A number's value, read from its source spelling rather than the parser's
/// `f32` field: SassScript arithmetic is `f64`, and dart-sass's output
/// precision depends on it.
pub(super) fn number(n: &ox::Number<'_>) -> f64 {
    n.raw.parse::<f64>().unwrap_or(n.value as f64)
}

/// A quoted string's contents, with the surrounding quotes removed. The raw
/// text is otherwise kept verbatim: an escape inside a string is part of how
/// it serializes.
pub(super) fn unquote_raw(raw: &str) -> &str {
    let b = raw.as_bytes();
    match (b.first(), b.last()) {
        (Some(b'"'), Some(b'"')) | (Some(b'\''), Some(b'\'')) if raw.len() >= 2 => {
            &raw[1..raw.len() - 1]
        }
        // A chunk of an interpolated string carries only the one delimiter it
        // touches, or none at all in the middle.
        (Some(b'"' | b'\''), _) => &raw[1..],
        (_, Some(b'"' | b'\'')) => &raw[..raw.len() - 1],
        _ => raw,
    }
}

/// The span of a call's argument text, between its parentheses.
pub(super) fn args_span(f: &ox::Function<'_>) -> Span {
    Span {
        start: f.name.span().end + 1,
        end: f.span.end.saturating_sub(1),
    }
}

/// Whether a value run is eligible to keep the deprecated `/` slash spelling.
///
/// dart-sass keeps the slash only between number literals (including a
/// unary-signed literal) and chains of such slash divisions; variables,
/// function calls, parentheses, and other operations force real division.
fn is_slash_operand(values: &[ox::ComponentValue<'_>]) -> bool {
    matches!(values, [one] if is_slash_operand_node(one))
}

fn is_slash_operand_node(v: &ox::ComponentValue<'_>) -> bool {
    use ox::ComponentValue as CV;
    match v {
        CV::Number(_) | CV::Percentage(_) | CV::Dimension(_) => true,
        // dart-sass also keeps the slash spelling when an operand is a
        // `calc()` (and the math-function family parsed as calculations):
        // `calc(1)/2` -> `1/2`, `calc(2px)/calc(4px)` -> `2px/4px`. The calc
        // operand folds to a number at eval time, so the slash repr is its
        // serialized value.
        CV::Function(f) => matches!(
            &f.name,
            ox::FunctionName::Ident(ox::InterpolableIdent::Literal(id))
                if id.name.eq_ignore_ascii_case("calc")
        ),
        CV::SassUnaryExpression(u) => {
            matches!(u.op.kind, ox::SassUnaryOperatorKind::Minus)
                && is_slash_operand_node(&u.expr)
        }
        CV::SassBinaryExpression(b) => {
            matches!(b.op.kind, ox::SassBinaryOperatorKind::Division)
                && is_slash_operand_node(&b.left)
                && is_slash_operand_node(&b.right)
        }
        CV::Ratio(_) => true,
        _ => false,
    }
}

/// Whether `name` is a global function with no plain-CSS meaning, which a
/// plain-CSS (`.css`) module may not call.
pub(super) fn is_sass_only_function(name: &str) -> bool {
    matches!(
        name,
        // list
        "index" | "nth" | "set-nth" | "join" | "append" | "zip" | "list-separator"
            | "is-bracketed" | "length"
            // map
            | "map-get" | "map-merge" | "map-remove" | "map-keys" | "map-values" | "map-has-key"
            // meta
            | "type-of" | "unit" | "unitless" | "comparable" | "inspect" | "keywords"
            | "feature-exists" | "variable-exists" | "global-variable-exists" | "function-exists"
            | "mixin-exists" | "content-exists" | "get-function" | "call" | "get-mixin"
            // string
            | "str-length" | "str-insert" | "str-index" | "str-slice" | "to-upper-case"
            | "to-lower-case" | "unique-id"
            // color (Sass-only adjusters / getters, not CSS functions)
            | "mix" | "adjust-hue" | "lighten" | "darken" | "desaturate" | "opacify"
            | "transparentize" | "fade-in" | "fade-out" | "scale-color" | "adjust-color"
            | "change-color" | "ie-hex-str"
            // selector
            | "selector-nest" | "selector-append" | "selector-replace" | "selector-unify"
            | "is-superselector" | "simple-selectors" | "selector-parse" | "selector-extend"
    )
}

/// Classify a function name as a "special" CSS function whose argument list is
/// preserved verbatim rather than evaluated as SassScript. Matching is
/// case-insensitive and the returned name is the dart-sass canonical
/// (lower-cased) spelling that is emitted.
///
/// `element` and `expression` are special with or without a single vendor
/// prefix, and so is `calc` — but only *with* one, since a bare `calc()` is a
/// real calculation. `type` is special only when *un*prefixed. `url`, `var`
/// and `env` are handled separately (they have their own raw paths).
pub(super) fn special_function_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let unprefixed = strip_vendor_prefix(&lower);
    let prefixed = unprefixed != lower;
    match unprefixed {
        "element" | "expression" => Some(lower),
        "calc" if prefixed => Some(lower),
        "type" if !prefixed => Some(lower),
        _ => None,
    }
}

/// Strip a single leading `-<vendor>-` prefix from an already-lower-cased
/// identifier, returning the remainder. A vendor prefix is `-`, one or more
/// identifier characters, then `-` (e.g. `-webkit-`). If no such prefix is
/// present the whole string is returned unchanged.
pub(super) fn strip_vendor_prefix(lower: &str) -> &str {
    if !lower.starts_with('-') {
        return lower;
    }
    match lower[1..].find('-') {
        Some(rel) if rel >= 1 => &lower[1 + rel + 1..],
        _ => lower,
    }
}
