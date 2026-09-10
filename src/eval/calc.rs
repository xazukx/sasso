use super::value_read::{is_solidus, Arg};
use super::*;

impl<'a> Evaluator<'a> {
    /// Serialize a CSS math function (`min`/`max`/`clamp`/…) verbatim inside a
    /// `@supports` declaration: each argument is resolved through the
    /// (non-folding) calc machinery and joined with `, `. Used only when
    /// `in_supports_declaration` is set.
    pub(super) fn eval_supports_calc_func(
        &mut self,
        name: &str,
        args: &[Arg<'a>],
        pos: Pos,
    ) -> Result<Value, Error> {
        if args.iter().any(|a| a.splat) {
            return Err(Error::at(
                "Rest arguments can't be used with calculations.",
                pos,
            ));
        }
        let mut parts = Vec::with_capacity(args.len());
        for a in args {
            let inner = self
                .eval_calc_values(a.values)?
                .to_calc_css(self.compressed());
            // A named argument (`min($a: 1)`) is not valid in a calculation,
            // but we preserve any name verbatim to mirror the surface syntax.
            match a.name {
                Some(n) => parts.push(format!("${n}: {inner}")),
                None => parts.push(inner),
            }
        }
        Ok(Value::Str(SassStr {
            text: format!("{name}({})", parts.join(", ")).into(),
            quoted: false,
        }))
    }

    /// Try to evaluate a single-/double-argument math calculation (`sin`,
    /// `sqrt`, `pow`, `log`, `hypot`, …) as a calculation. Each argument is
    /// evaluated through the calc machinery, which rejects disallowed
    /// operators (`%`, comparisons) the way dart-sass does.
    ///
    /// - When *every* argument folds to a single number, returns `Ok(None)`:
    ///   the caller falls through to the ordinary builtin, which computes the
    ///   result and applies its unit checks (so `sqrt(2)`, `sin(1deg)`,
    ///   `sin(1px)`-the-error all behave exactly as before).
    /// - When an argument still carries an opaque operand — a `var()`,
    ///   interpolation, or unknown identifier — the whole call is preserved as
    ///   a calculation string (`sin(2px + var(--c))`).
    /// - When an argument reduces to a numeric operation we cannot collapse to
    ///   a single number (compound/inverse units), returns `Ok(None)` so the
    ///   builtin re-evaluates and reports its own error, rather than silently
    ///   preserving a value dart-sass would reject.
    pub(super) fn try_eval_calc_math_call(
        &mut self,
        name: &str,
        args: &[Arg<'a>],
        _pos: Pos,
    ) -> Result<Option<Value>, Error> {
        let mut nodes = Vec::with_capacity(args.len());
        for a in args {
            nodes.push(self.eval_calc_values(a.values)?);
        }
        // Every argument is a plain number: let the builtin compute.
        if nodes.iter().all(|n| matches!(n, CalcNode::Number(_))) {
            return Ok(None);
        }
        // No opaque operand anywhere: the calculation is purely numeric but
        // did not collapse (compound/inverse units). Defer to the builtin so
        // it can raise the dart-sass error instead of us preserving an invalid
        // value.
        if !nodes.iter().any(calc_node_has_opaque) {
            return Ok(None);
        }
        let lname = name.to_ascii_lowercase();
        let parts: Vec<String> = nodes
            .iter()
            .map(|n| n.to_calc_css(self.compressed()))
            .collect();
        Ok(Some(Value::Str(SassStr {
            text: format!("{lname}({})", parts.join(", ")).into(),
            quoted: false,
        })))
    }

    /// Evaluate a three-argument `clamp(min, value, max)` calculation. Each
    /// argument is evaluated through the calc machinery (rejecting `%` and
    /// other non-calculation operators). When every argument folds to a single
    /// number, the builtin clamps/unit-checks them as before; when an argument
    /// keeps a `var()`/calculation operand the call is preserved
    /// (`clamp(1% + 1px, 2px, 3px)`). A resolved operand with complex units is
    /// rejected like dart-sass.
    pub(super) fn try_eval_clamp(&mut self, args: &[Arg<'a>], pos: Pos) -> Result<Value, Error> {
        let mut nodes = Vec::with_capacity(args.len());
        for a in args {
            let node = self.eval_calc_values(a.values)?;
            if let Some(complex) = calc_complex_unit_operand(&node) {
                return Err(Error::at(
                    format!(
                        "Number calc({}) isn't compatible with CSS calculations.",
                        complex.to_calc_css(false)
                    ),
                    pos,
                ));
            }
            nodes.push(node);
        }
        // Every argument is a plain number: let the builtin clamp them (and
        // run its incompatible-unit and complex-unit checks).
        if nodes.iter().all(|n| matches!(n, CalcNode::Number(_))) {
            let values: Vec<Value> = nodes
                .into_iter()
                .map(|n| match n {
                    CalcNode::Number(num) => Value::Number(num),
                    // Unreachable: guarded by the `all` check above.
                    other => Value::Calc(other),
                })
                .collect();
            return crate::builtins::call("clamp", &values, &[], pos);
        }
        let parts: Vec<String> = nodes
            .iter()
            .map(|n| n.to_calc_css(self.compressed()))
            .collect();
        Ok(Value::Str(SassStr {
            text: format!("clamp({})", parts.join(", ")).into(),
            quoted: false,
        }))
    }

    /// Evaluate a `calc-size(target, value)` calculation. The target (`auto`,
    /// `none`, `size`, a `var()`, or a nested calculation) and the optional
    /// value are each evaluated through the calc machinery and the call is
    /// kept preserved (`calc-size()` never reduces to a number). Exactly one
    /// or two arguments are accepted.
    pub(super) fn eval_calc_size(&mut self, args: &[Arg<'a>], pos: Pos) -> Result<Value, Error> {
        if args.is_empty() {
            return Err(Error::at("Missing argument.", pos));
        }
        if args.len() > 2 {
            return Err(Error::at(
                format!("Only 2 arguments allowed, but {} were passed.", args.len()),
                pos,
            ));
        }
        let mut parts = Vec::with_capacity(args.len());
        for a in args {
            parts.push(
                self.eval_calc_values(a.values)?
                    .to_calc_css(self.compressed()),
            );
        }
        Ok(Value::Str(SassStr {
            text: format!("calc-size({})", parts.join(", ")).into(),
            quoted: false,
        }))
    }

    /// Evaluate a run of component values as a calculation.
    ///
    /// The parser reports a calculation's arithmetic as [`ox::Calc`] nodes, so
    /// a run reaching here is either one such node, a `/`-joined chain, or a
    /// space-separated "unparsed" run (see below).
    pub(super) fn eval_calc_values(
        &mut self,
        values: &'a [ox::ComponentValue<'a>],
    ) -> Result<CalcNode, Error> {
        self.calc_depth += 1;
        let result = self.eval_calc_values_inner(values);
        self.calc_depth -= 1;
        result
    }

    fn eval_calc_values_inner(
        &mut self,
        values: &'a [ox::ComponentValue<'a>],
    ) -> Result<CalcNode, Error> {
        if let [one] = values {
            return self.eval_calc_value(one);
        }
        // A `/` chain the parser did not fold into a `Calc` node.
        if values.iter().any(is_solidus) {
            let mut parts: Vec<&'a [ox::ComponentValue<'a>]> = Vec::new();
            let mut ops: Vec<Span> = Vec::new();
            let mut start = 0usize;
            for (i, v) in values.iter().enumerate() {
                if is_solidus(v) {
                    parts.push(&values[start..i]);
                    ops.push(*v.span());
                    start = i + 1;
                }
            }
            parts.push(&values[start..]);
            let mut iter = parts.into_iter();
            let mut acc = self.eval_calc_values_inner(iter.next().unwrap_or(&[]))?;
            for (rhs, op) in iter.zip(ops) {
                let r = self.eval_calc_values_inner(rhs)?;
                acc = if self.in_supports_declaration {
                    CalcNode::Op {
                        op: CalcOp::Div,
                        left: Box::new(acc),
                        right: Box::new(r),
                    }
                } else {
                    fold_calc(CalcOp::Div, acc, r, self.file.pos(op))?
                };
            }
            return Ok(acc);
        }
        if values.is_empty() {
            return Err(Error::unpositioned("Expected expression."));
        }

        // A space-separated run written directly in the calc interior is an
        // "unparsed" run: it is only valid when it contains a `var()`/`env()`
        // substitution, an interpolation, or a variable holding an unquoted
        // string — all of which dart-sass splices verbatim (`calc(var(--c) 1)`,
        // `calc(#{"1 +"} 2)` -> `calc(1 + 2)`, `calc(1 $c)` with
        // `$c: unquote("+ 2")` -> `calc(1 + 2)`). A space run of ordinary
        // operands (`calc(1 2)`, `calc(c 1 2)`) or of number-valued variables
        // (`calc(1 $n)`) has no operator between adjacent terms:
        // "Missing math operator.".
        let has_subst = values.iter().any(value_has_substitution);
        let has_var = values.iter().any(|v| {
            matches!(
                v,
                ox::ComponentValue::SassVariable(_) | ox::ComponentValue::SassQualifiedName(_)
            )
        });
        if !has_subst && !has_var {
            return Err(Error::unpositioned("Missing math operator."));
        }
        let mut parts = Vec::with_capacity(values.len());
        let mut any_str = false;
        for v in values {
            let node = self.eval_calc_value(v)?;
            if matches!(node, CalcNode::Str(_)) {
                any_str = true;
            }
            parts.push(node.to_calc_css(false));
        }
        // Variables alone only justify the splice when at least one resolved
        // to raw text.
        if !has_subst && !any_str {
            return Err(Error::unpositioned("Missing math operator."));
        }
        Ok(CalcNode::Str(parts.join(" ")))
    }

    /// Evaluate one component value as a calculation operand.
    ///
    /// Numeric `number <op> number` subtrees with compatible units fold;
    /// everything else (variables, interpolations, incompatible units) is
    /// preserved for canonical serialization, mirroring dart-sass's
    /// "only simplify number+number" rule.
    fn eval_calc_value(&mut self, value: &'a ox::ComponentValue<'a>) -> Result<CalcNode, Error> {
        use ox::ComponentValue as CV;
        match value {
            CV::Calc(c) => self.eval_calc_op(c),
            CV::SassBinaryExpression(b) => {
                use ox::SassBinaryOperatorKind as K;
                let pos = self.file.pos(b.op.span);
                let calc_op = match b.op.kind {
                    K::Plus => CalcOp::Add,
                    K::Minus => CalcOp::Sub,
                    K::Multiply => CalcOp::Mul,
                    K::Division => CalcOp::Div,
                    // Modulo, comparisons, and `and`/`or` are not arithmetic;
                    // dart-sass rejects them inside a calculation rather than
                    // evaluating them (`calc(1px % 2px)`, `calc(1 > 2)`).
                    _ => {
                        return Err(Error::at(
                            "This operation can't be used in a calculation.",
                            pos,
                        ));
                    }
                };
                let l = self.eval_calc_value(&b.left)?;
                let r = self.eval_calc_value(&b.right)?;
                if self.in_supports_declaration {
                    return Ok(CalcNode::Op {
                        op: calc_op,
                        left: Box::new(l),
                        right: Box::new(r),
                    });
                }
                fold_calc(calc_op, l, r, pos)
            }
            CV::SassUnaryExpression(u) if matches!(u.op.kind, ox::SassUnaryOperatorKind::Minus) => {
                let node = self.eval_calc_value(&u.expr)?;
                match node {
                    CalcNode::Number(n) => Ok(CalcNode::Number(n.copy_units(-n.value))),
                    other => Ok(CalcNode::Op {
                        op: CalcOp::Mul,
                        left: Box::new(CalcNode::Number(Number::unitless(-1.0))),
                        right: Box::new(other),
                    }),
                }
            }
            // Parentheses around a single opaque operand (a `var()`,
            // identifier, or interpolation) are preserved verbatim; around a
            // number or an operation they are redundant and dropped (operator
            // precedence reintroduces them where needed).
            CV::SassParenthesizedExpression(p) => {
                let node = self.eval_calc_value(&p.expr)?;
                match node {
                    CalcNode::Str(s) => Ok(CalcNode::Str(format!("({s})"))),
                    other => Ok(other),
                }
            }
            // A nested calc() flattens into the surrounding calculation —
            // except inside a `@supports` declaration, where dart-sass keeps
            // the calculation unsimplified, so the inner `calc(...)` stays
            // wrapped. Otherwise an unresolved single-string operand (an
            // interpolation or a `var()` substitution that is not a clean
            // operand) is parenthesized: dart-sass writes `calc(calc(#{"c*"}))`
            // as `calc((c*))` and `calc(1 + calc(var(--c)))` as
            // `calc(1 + (var(--c)))`. A clean identifier, number, operation, or
            // complete sub-calculation flattens without extra parens.
            CV::Function(f) if is_bare_calc(f) && !f.args.is_empty() => {
                if self.in_supports_declaration {
                    let s = self
                        .eval_calc_values(&f.args)?
                        .to_calc_css(self.compressed());
                    return Ok(CalcNode::Str(format!("calc({s})")));
                }
                let node = self.eval_calc_values(&f.args)?;
                match node {
                    CalcNode::Str(s) if nested_calc_needs_parens(&s) => {
                        Ok(CalcNode::Str(format!("({s})")))
                    }
                    other => Ok(other),
                }
            }
            // Any leaf (number, var(), interpolation, ident) evaluates to a
            // value and becomes a calc operand.
            other => {
                let v = self.eval_value(other)?;
                // The calc constants `pi`/`e`/`infinity`/`-infinity`/`nan`
                // (case-insensitive) resolve to their numeric values inside a
                // calculation, so `calc(infinity * 2)` folds to
                // `calc(infinity)` and `calc(NaN)` canonicalizes its spelling.
                if let Value::Str(s) = &v {
                    if !s.quoted {
                        if let Some(value) = calc_constant(&s.text) {
                            return Ok(CalcNode::Number(Number::unitless(value)));
                        }
                    }
                }
                // Only a number, a nested calculation, or an unquoted string
                // (a `var()`, interpolation, ident, or other special CSS
                // value) is a legal calculation operand. A null, bool, color,
                // list, map, or quoted string evaluated into the calc —
                // typically via a `$variable` or function call — is rejected
                // the way dart-sass does.
                match &v {
                    Value::Number(_) | Value::Calc(_) | Value::Slash(_, _) => {}
                    Value::Str(s) if !s.quoted => {}
                    other => {
                        return Err(Error::unpositioned(format!(
                            "Value {} can't be used in a calculation.",
                            calc_value_repr(other)
                        )));
                    }
                }
                Ok(value_to_calc_node(v))
            }
        }
    }

    /// A `Calc` operation node, which is how the parser reports arithmetic
    /// inside a calculation.
    pub(super) fn eval_calc_op(&mut self, c: &'a ox::Calc<'a>) -> Result<CalcNode, Error> {
        let pos = self.file.pos(c.op.span);
        let calc_op = match c.op.kind {
            ox::CalcOperatorKind::Plus => CalcOp::Add,
            ox::CalcOperatorKind::Minus => CalcOp::Sub,
            ox::CalcOperatorKind::Multiply => CalcOp::Mul,
            ox::CalcOperatorKind::Division => CalcOp::Div,
            // dart-sass rejects `%` inside a calculation rather than
            // evaluating it (`calc(1px % 2px)`).
            ox::CalcOperatorKind::Modulo => {
                return Err(Error::at(
                    "This operation can't be used in a calculation.",
                    pos,
                ));
            }
        };
        let l = self.eval_calc_value(&c.left)?;
        let r = self.eval_calc_value(&c.right)?;
        if self.in_supports_declaration {
            return Ok(CalcNode::Op {
                op: calc_op,
                left: Box::new(l),
                right: Box::new(r),
            });
        }
        fold_calc(calc_op, l, r, pos)
    }

    /// Whether a value run contains a calculation substitution — a
    /// `var()`/`env()` reference or an interpolation — anywhere within it.
    ///
    /// Used to decide whether a legacy global math function such as `abs()` is
    /// being used as a CSS calculation (so its argument is preserved) rather
    /// than as the deprecated Sass global.
    pub(super) fn values_contain_calc_substitution(
        &self,
        values: &'a [ox::ComponentValue<'a>],
    ) -> bool {
        values.iter().any(value_contains_calc_substitution)
    }

    /// Whether a value run contains a SassScript-only operator — one that has
    /// no meaning inside a calculation, so the call is a plain function rather
    /// than a CSS calculation.
    pub(super) fn values_have_non_calc_op(&self, values: &'a [ox::ComponentValue<'a>]) -> bool {
        values.iter().any(value_has_non_calc_op)
    }
}

/// Whether a function node is an unprefixed `calc(`.
fn is_bare_calc(f: &ox::Function<'_>) -> bool {
    matches!(
        &f.name,
        ox::FunctionName::Ident(ox::InterpolableIdent::Literal(id))
            if id.name.eq_ignore_ascii_case("calc")
    )
}

/// Whether a value is itself a calculation substitution: an interpolation, or
/// a `var()`/`env()` reference (both are parsed as plain function calls, and
/// inside a calc space-run they are legal substitutions).
fn value_has_substitution(v: &ox::ComponentValue<'_>) -> bool {
    use ox::ComponentValue as CV;
    match v {
        CV::Function(f) => matches!(
            &f.name,
            ox::FunctionName::Ident(ox::InterpolableIdent::Literal(id))
                if id.name.eq_ignore_ascii_case("var") || id.name.eq_ignore_ascii_case("env")
        ),
        CV::InterpolableIdent(ox::InterpolableIdent::SassInterpolated(_)) => true,
        CV::InterpolableStr(ox::InterpolableStr::SassInterpolated(_)) => true,
        _ => false,
    }
}

/// [`value_has_substitution`], recursing through operations, parentheses,
/// nested calculations and lists.
fn value_contains_calc_substitution(v: &ox::ComponentValue<'_>) -> bool {
    use ox::ComponentValue as CV;
    if value_has_substitution(v) {
        return true;
    }
    match v {
        CV::Calc(c) => {
            value_contains_calc_substitution(&c.left) || value_contains_calc_substitution(&c.right)
        }
        CV::SassBinaryExpression(b) => {
            value_contains_calc_substitution(&b.left) || value_contains_calc_substitution(&b.right)
        }
        CV::SassUnaryExpression(u) => value_contains_calc_substitution(&u.expr),
        CV::SassParenthesizedExpression(p) => value_contains_calc_substitution(&p.expr),
        CV::SassList(l) => l.elements.iter().any(value_contains_calc_substitution),
        CV::BracketBlock(b) => b.value.iter().any(value_contains_calc_substitution),
        CV::Function(f) => f.args.iter().any(value_contains_calc_substitution),
        _ => false,
    }
}

/// Whether a value contains an operator with no meaning inside a calculation
/// (`%`, a comparison, `and`/`or`).
fn value_has_non_calc_op(v: &ox::ComponentValue<'_>) -> bool {
    use ox::ComponentValue as CV;
    use ox::SassBinaryOperatorKind as K;
    match v {
        CV::SassBinaryExpression(b) => {
            matches!(
                b.op.kind,
                K::Modulo
                    | K::EqualsEquals
                    | K::ExclamationEquals
                    | K::LessThan
                    | K::GreaterThan
                    | K::LessThanOrEqual
                    | K::GreaterThanOrEqual
                    | K::And
                    | K::Or
            ) || value_has_non_calc_op(&b.left)
                || value_has_non_calc_op(&b.right)
        }
        CV::Calc(c) => {
            matches!(c.op.kind, ox::CalcOperatorKind::Modulo)
                || value_has_non_calc_op(&c.left)
                || value_has_non_calc_op(&c.right)
        }
        CV::SassUnaryExpression(u) => value_has_non_calc_op(&u.expr),
        CV::SassParenthesizedExpression(p) => value_has_non_calc_op(&p.expr),
        CV::SassList(l) => l.elements.iter().any(value_has_non_calc_op),
        CV::BracketBlock(b) => b.value.iter().any(value_has_non_calc_op),
        _ => false,
    }
}
