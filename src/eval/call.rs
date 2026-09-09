//! Dispatching an unqualified function call.
//!
//! The resolution order is dart-sass's: a user `@function` first, then a
//! member exposed unprefixed by `@use … as *`, then the CSS calculations
//! (which parse their arguments as calculations rather than SassScript), then
//! host-defined custom functions, then the global builtins.

use super::value_read::Arg;
use super::*;

impl<'a> Evaluator<'a> {
    /// Evaluate a call to `name` with already-grouped arguments.
    pub(super) fn eval_call(
        &mut self,
        name: &str,
        args: &[Arg<'a>],
        pos: Pos,
        length: usize,
    ) -> Result<Value, Error> {
        // In a plain-CSS module no function is invoked (dart-sass `plainCss`
        // looks none up): the call re-serializes verbatim with its arguments
        // evaluated. CSS calculations (min/max/…) still simplify through their
        // normal paths below.
        if self.in_plain_css
            && !is_supports_calc_function(name)
            && !name.eq_ignore_ascii_case("calc")
            && args.iter().all(|a| a.is_plain())
        {
            let mut parts: Vec<String> = Vec::with_capacity(args.len());
            for a in args {
                parts.push(self.eval_values(a.values)?.to_css(self.compressed()));
            }
            return Ok(Value::Str(SassStr {
                text: format!("{name}({})", parts.join(", ")).into(),
                quoted: false,
            }));
        }

        // Resolve a user `@function` of this name ONCE (it is probed many
        // times along this arm). A `--`-prefixed call is always plain CSS
        // (dart reserves it for custom functions), so it never binds to a user
        // function even though `@function __a` would normalize to the same
        // `--a` key. Every later "no user override" guard reuses `has_user_fn`.
        let user_fn = if name.starts_with("--") {
            None
        } else {
            self.lookup_function(name)
        };
        let has_user_fn = user_fn.is_some();

        // Inside a `@supports` declaration, a CSS math function
        // (`min`/`max`/`clamp`/…) is kept unsimplified: its arguments are
        // resolved through the (non-folding) calc machinery and the call is
        // serialized verbatim, matching dart-sass `simplify: false`. A
        // user-defined function of the same name still wins, so this only
        // applies to builtins.
        if self.in_supports_declaration && is_supports_calc_function(name) && !has_user_fn {
            return self.eval_supports_calc_func(name, args, pos);
        }
        // if() is lazy: only the selected branch is evaluated.
        if name == "if" {
            return self.eval_if_function(args, pos);
        }
        // User-defined @function takes precedence over builtins.
        if let Some(func) = user_fn {
            return self.call_function(&func, args, Some((pos, length)));
        }
        // A user module function exposed unprefixed via `@use … as *`.
        if !self.star_user_modules.is_empty() && !is_private_member(name) {
            let hits: Vec<(Rc<Module<'a>>, Rc<UserCallable<'a>>)> = self
                .star_user_modules
                .iter()
                .filter_map(|m| m.function(name).map(|f| (Rc::clone(m), f)))
                .collect();
            if hits.len() > 1 {
                return Err(Error::at(
                    "This function is available from multiple global modules.".to_string(),
                    pos,
                ));
            }
            if let Some((m, f)) = hits.into_iter().next() {
                return self.call_user_module_function(&m, &f, args, Some((pos, length)));
            }
        }
        // A bare `calc()` reaches here as a plain call (the parser only treats
        // `calc(<arg>)` as a calculation), so a user `@function calc()` could
        // have handled it above. With no user override it is the CSS `calc()`,
        // which requires an argument.
        if name.eq_ignore_ascii_case("calc") && args.is_empty() {
            return Err(Error::at("Missing argument.", pos));
        }
        // The pure CSS-calculation functions are parsed as calculations, which
        // cannot take a `...` rest argument.
        if is_calc_function(name) && args.iter().any(|a| a.splat) {
            return Err(Error::at(
                "Rest arguments can't be used with calculations.",
                pos,
            ));
        }
        // The single-/double-argument math calculations (`sin`, `cos`, `sqrt`,
        // `pow`, `log`, `hypot`, …) parse their arguments as calculation
        // expressions, not ordinary SassScript. That means a disallowed
        // operator (`%`, comparison) inside an argument is rejected, and an
        // argument that does not reduce to a single number — it still
        // references a `var()`/interpolation/unknown ident — keeps the whole
        // call as a preserved calculation (`sin(2px + var(--c))`) rather than
        // erroring. When every argument reduces to a plain number the normal
        // builtin path computes the result (and applies its unit checks), so
        // this only changes the two calc-specific behaviours above.
        if is_pure_calc_math_function(name) && !has_user_fn && args.iter().all(|a| a.is_plain())
            && let Some(v) = self.try_eval_calc_math_call(name, args, pos)?
        {
            return Ok(v);
        }
        // `calc-size()` is a two-argument calculation: a sizing keyword (or
        // `var()`/calculation) plus a calculation, always preserved.
        if name.eq_ignore_ascii_case("calc-size")
            && !has_user_fn
            && args.iter().all(|a| a.is_plain())
        {
            return self.eval_calc_size(args, pos);
        }
        // A three-argument `clamp()` evaluates its bounds and value as
        // calculations, so a `var()`/operation argument (`1% + 1px`) keeps the
        // call preserved instead of erroring as Sass arithmetic. Other arities
        // (a preserved single argument, or an arity error) fall through to the
        // builtin.
        if name.eq_ignore_ascii_case("clamp")
            && !has_user_fn
            && args.len() == 3
            && args.iter().all(|a| a.is_plain())
        {
            return self.try_eval_clamp(args, pos);
        }
        // `abs()` is a legacy global function that also exists as the CSS
        // `abs()` calculation. When its single positional argument references
        // a `var()`/interpolation it is parsed as a calculation and preserved
        // with its numeric subtree folded (`abs(1px + 2px - var(--c))` ->
        // `abs(3px - var(--c))`). Without such a substitution the argument
        // resolves to a plain number, so the deprecated `math.abs` global
        // handles it as before (`abs(1 + 1px)` -> `2px`, `abs(-3)` -> `3`).
        if name.eq_ignore_ascii_case("abs")
            && !has_user_fn
            && args.len() == 1
            && args[0].is_plain()
            && self.values_contain_calc_substitution(args[0].values)
        {
            let node = self.eval_calc_values(args[0].values)?;
            return Ok(Value::Str(SassStr {
                text: format!("abs({})", node.to_calc_css(self.compressed())).into(),
                quoted: false,
            }));
        }
        // `round()` is likewise the CSS round() calculation: each argument
        // evaluates as a calculation (so `1.4px + var(--c)` keeps its `+` with
        // the numeric subtree folded, and `1px + 4px` folds to `5px`). If
        // every argument simplifies (numbers, plus a leading strategy
        // keyword), the normal builtin computes the result; otherwise the call
        // is preserved. Calc-incompatible SassScript (`7 % 3`) falls back to
        // the legacy one-argument `math.round` (arity errors and all).
        if name.eq_ignore_ascii_case("round")
            && !has_user_fn
            && (1..=3).contains(&args.len())
            && args.iter().all(|a| a.is_plain())
        {
            // A SassScript-only operator (`7 % 3`) makes the call a plain
            // function, and the only plain `round` is the legacy one-argument
            // `math.round` — multi-argument forms are an arity error.
            if args.iter().any(|a| self.values_have_non_calc_op(a.values)) {
                if args.len() > 1 {
                    return Err(Error::at(
                        format!("Only 1 argument allowed, but {} were passed.", args.len()),
                        pos,
                    ));
                }
                // A single argument falls through to the legacy builtin.
            } else if let Ok(nodes) = args
                .iter()
                .map(|a| self.eval_calc_values(a.values))
                .collect::<Result<Vec<CalcNode>, Error>>()
            {
                let simplified = |i: usize, n: &CalcNode| match n {
                    CalcNode::Number(_) => true,
                    // A leading strategy keyword participates as a keyword,
                    // not an operand.
                    CalcNode::Str(s) => {
                        i == 0
                            && nodes.len() >= 2
                            && matches!(
                                s.to_ascii_lowercase().as_str(),
                                "nearest" | "up" | "down" | "to-zero"
                            )
                    }
                    _ => false,
                };
                if !nodes.iter().enumerate().all(|(i, n)| simplified(i, n)) {
                    // A preserved round is a first-class Calculation
                    // (`meta.calc-name`/`calc-args` see it).
                    return Ok(Value::Calc(CalcNode::Func {
                        name: "round".to_string(),
                        args: nodes,
                    }));
                }
                // Fall through to the normal builtin path below.
            }
        }
        // `min`/`max` are likewise CSS calculations: their arguments fold as
        // calc expressions, and any unsimplifiable operand (`1px + var(--c)`)
        // preserves the call with the folded subtrees spelled as calc
        // (`max(5px, 1px + var(--c))`). When every argument folds to a number
        // the builtin computes as before, and SassScript-only operators
        // (`7 % 3`) keep the whole call on the legacy path.
        if (name.eq_ignore_ascii_case("min") || name.eq_ignore_ascii_case("max"))
            && !has_user_fn
            && !args.is_empty()
            && args.iter().all(|a| a.is_plain())
            && !args.iter().any(|a| self.values_have_non_calc_op(a.values))
            && let Ok(nodes) = args
                .iter()
                .map(|a| self.eval_calc_values(a.values))
                .collect::<Result<Vec<CalcNode>, Error>>()
            && !nodes.iter().all(|n| matches!(n, CalcNode::Number(_)))
        {
            // A preserved min/max is a first-class Calculation
            // (`meta.calc-name`/`calc-args` see it).
            return Ok(Value::Calc(CalcNode::Func {
                name: name.to_ascii_lowercase(),
                args: nodes,
            }));
        }

        // Evaluate args, expanding any `...` splat into positional / keyword
        // arguments.
        let (mut pos_args, mut named, call_sep) = self.eval_call_args(args)?;
        // The global (deprecated) aliases of the `sass:meta` existence
        // predicates resolve against the evaluator state, not the value-only
        // builtin layer. A user-defined function of the same name still wins
        // (checked above).
        if matches!(
            name,
            "variable-exists"
                | "global-variable-exists"
                | "mixin-exists"
                | "function-exists"
                | "content-exists"
                | "get-function"
                | "call"
                | "keywords"
        ) {
            without_slash_all(&mut pos_args, &mut named);
            if let Some(r) = self.try_meta_eval_call(name, &pos_args, &named, pos) {
                return r;
            }
        }
        // The proprietary Microsoft `alpha()` filter overload: when the global
        // `alpha()` is called with one or more unquoted-string arguments that
        // each contain a `=` (an IE `alpha(opacity=80)` hack, produced by the
        // single-`=` operator), dart-sass passes the call through verbatim as
        // a CSS function instead of treating the argument as a color.
        if name == "alpha"
            && named.is_empty()
            && !pos_args.is_empty()
            && pos_args
                .iter()
                .all(|v| matches!(v, Value::Str(s) if !s.quoted && s.text.contains('=')))
        {
            let inner = pos_args
                .iter()
                .map(|v| v.to_css(false))
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(Value::Str(SassStr {
                text: format!("alpha({inner})").into(),
                quoted: false,
            }));
        }
        // A member exposed unprefixed via `@use "sass:<mod>" as *`: when the
        // bare name is not already a global builtin, route it to the first
        // star module that owns it (e.g. `div` -> `math.div`, `set` ->
        // `map.set`). Global builtins keep their own behaviour.
        if !crate::builtins::is_builtin(name) {
            for m in self.star_modules.clone() {
                if crate::builtins::module_has_member(&m, name) {
                    without_slash_all(&mut pos_args, &mut named);
                    return crate::builtins::call_module(&m, name, &pos_args, &named, pos)
                        .map(Value::without_slash);
                }
            }
        }
        // Host-defined custom functions (dart-sass `functions`): they override
        // built-in globals but lose to user `@function`s and module members
        // (checked above). Resolved by calling back into the embedder with the
        // bound args serialized to the host-value wire format.
        if !self.options.functions.is_empty() {
            let norm = crate::host_fn::normalize_name(name);
            if let Some(idx) = self.options.functions.iter().position(|f| f.name == norm) {
                // Extract the owned signature + callback so the immutable
                // borrow on `self.options` ends before any `&mut self`.
                let (param_names, rest_name): (Vec<String>, Option<String>) = {
                    let hf = &self.options.functions[idx];
                    match &hf.params {
                        Ok(p) => (
                            p.params.iter().map(|x| x.name.clone()).collect(),
                            p.rest.clone(),
                        ),
                        Err(e) => return Err(Error::at(e.clone(), pos)),
                    }
                };
                let callback = self.options.functions[idx].callback.clone();
                // Real Sass function: collapse slash-division args.
                without_slash_all(&mut pos_args, &mut named);
                let bound = crate::host_fn::bind_host_args(
                    &param_names,
                    rest_name.as_deref(),
                    std::mem::take(&mut pos_args),
                    std::mem::take(&mut named),
                    call_sep,
                    &norm,
                )
                .map_err(|e| Error::at(e, pos))?;
                // First-class function/mixin args round-trip as opaque handles
                // into a per-dispatch table; save/restore the outer table so a
                // nested custom-function call is safe.
                let saved = crate::host_fn::swap_handles(Vec::new());
                let result: Result<Value, String> = crate::host_fn::serialize_args(&bound)
                    .and_then(|b| callback(&b))
                    .and_then(|b| crate::host_fn::deserialize_value(&b));
                crate::host_fn::swap_handles(saved);
                return result.map_err(|e| Error::at(e, pos)).map(Value::without_slash);
            }
        }
        // A bare slash-division argument collapses to its number when passed
        // to a real Sass function (dart-sass `withoutSlash`); plain CSS
        // functions (`foo(1/2)`) keep the slash verbatim.
        if crate::builtins::is_builtin(name) {
            without_slash_all(&mut pos_args, &mut named);
        }
        // A function call's RESULT is slash-free too (dart applies
        // `withoutSlash()` to every call result): `list.nth(3 1/2 4, 2)`
        // returns 0.5, not the slash form.
        crate::builtins::call(name, &pos_args, &named, pos).map(Value::without_slash)
    }
}

/// Collapse a bare slash-division in every bound argument.
fn without_slash_all(pos_args: &mut [Value], named: &mut [(String, Value)]) {
    for v in pos_args.iter_mut() {
        *v = std::mem::replace(v, Value::Null).without_slash();
    }
    for (_, v) in named.iter_mut() {
        *v = std::mem::replace(v, Value::Null).without_slash();
    }
}
