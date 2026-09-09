use super::value_read::values_span;
use super::*;

impl<'a> Evaluator<'a> {
    /// Evaluate a generic at-rule. The prelude template is resolved to a
    /// string; the body (when present) is executed so that nested rules carry
    /// the enclosing selectors INSIDE the at-rule, and the whole node hoists to
    /// the document root (bubbling).
    pub(super) fn eval_at_rule(
        &mut self,
        name: &str,
        prelude: Option<Span>,
        body: Option<&'a ox::SimpleBlock<'a>>,
        lines: SrcLines,
        parents: &[String],
        sink: &mut Sink<'_>,
    ) -> Result<(), Error> {
        let prelude = self.prelude_text(prelude)?;
        let Some(block) = body else {
            // dart-sass strips a top-level (or bubbled-out) `@charset` entirely,
            // but keeps one that appears inside a style rule's block.
            if name == "charset" && !sink.is_rule() {
                return Ok(());
            }
            sink.push_childless_at_rule(name.to_string(), prelude, lines);
            return Ok(());
        };
        // `@font-face` (exactly, case-sensitively, unprefixed) holds plain
        // declarations: dart-sass does NOT carry the enclosing style-rule
        // selector into its body — `a { @font-face { d: e } }` emits a bare
        // `@font-face { d: e }`. Every other at-rule (including `@page`,
        // `@-moz-font-face`, and unknown directives) wraps its body in the
        // enclosing selector — UNLESS we're directly inside `@at-root`,
        // where there is no style rule to wrap with (dart's _styleRule is
        // null even though `&` still resolves).
        let body_parents: &[String] = if name == "font-face" || self.at_root_excluding_style_rule {
            &[]
        } else {
            parents
        };
        // dart `_inUnknownAtRule`: every at-rule without a dedicated visitor
        // (this generic path) sets the flag for its body, legalizing bare
        // declarations in nested contexts that would otherwise reject them
        // (a `@media` directly inside `@utility`).
        let saved_unknown = std::mem::replace(&mut self.in_unknown_at_rule, true);
        let out_body = self.eval_at_body(block, body_parents);
        self.in_unknown_at_rule = saved_unknown;
        let out_body = out_body?;
        sink.push_at_rule(OutNode::AtRule {
            name: name.to_string(),
            prelude,
            body: out_body,
            has_block: true,
            lines,
        });
        Ok(())
    }

    /// An at-rule prelude's text, with `#{...}` resolved. An at-rule with no
    /// prelude at all (`@font-face`) has empty text.
    pub(super) fn prelude_text(&mut self, prelude: Option<Span>) -> Result<String, Error> {
        match prelude {
            // dart-sass re-serializes a prelude rather than echoing it: comments
            // are dropped and whitespace runs collapse to one space.
            Some(span) => Ok(collapse_prelude(&self.interpolate(span)?)),
            None => Ok(String::new()),
        }
    }

    /// Run an at-rule body, producing its output node list. When the at-rule
    /// is nested under a style rule, bare declarations are wrapped in the
    /// enclosing selectors; at the document root they emit directly.
    fn eval_at_body(
        &mut self,
        block: &'a ox::SimpleBlock<'a>,
        parents: &[String],
    ) -> Result<Vec<OutNode>, Error> {
        self.push_scope(false);
        // The at-rule body's trailing-invisible tracking is its own; the
        // exec arm that called us reports the at-rule itself to the parent.
        let saved_last_invisible = std::mem::replace(&mut self.last_child_invisible, false);
        let mut body: Vec<OutNode> = Vec::new();
        let result = if parents.is_empty() {
            let mut child = Sink::AtRoot {
                body: &mut body,
                group_ends: false,
            };
            self.exec(&block.statements, block.span, &[], &mut child)
        } else {
            let mut items: Vec<OutItem> = Vec::new();
            let mut nested: Vec<OutNode> = Vec::new();
            let mut flushed: Option<usize> = None;
            let at_depth = self.at_rule_ctx.len();
            // The wrap re-emits the enclosing selectors, and dart preserves
            // their source line structure inside the hoisted at-rule copy too
            // (`h1,\n.alpha { @include has-media; }`) — carry the enclosing
            // rule's linebreaks when they line up with `parents`.
            let wrap_linebreaks: Vec<bool> = if self.current_linebreaks.len() == parents.len() {
                self.current_linebreaks.clone()
            } else {
                Vec::new()
            };
            let res = {
                let mut child = Sink::Rule {
                    selectors: parents,
                    linebreaks: &wrap_linebreaks,
                    // The wrap re-uses the enclosing selectors, so it has no
                    // source rule of its own: `file`/`start`/`end` stay 0 to keep
                    // the trailing-comment rule disabled. For SOURCE MAPS only it
                    // carries the enclosing rule's selector position in the
                    // map-override fields, so the bubbled parent selector maps
                    // back to the ORIGINAL rule's span (dart parity). No CSS effect.
                    lines: SrcLines {
                        map_file: self.cur_rule_lines.file,
                        map_line: self.cur_rule_lines.start,
                        start_col: self.cur_rule_lines.start_col,
                        ..SrcLines::default()
                    },
                    items: &mut items,
                    nested: &mut nested,
                    at_depth,
                    flushed: &mut flushed,
                    // Inherit the enclosing rule's addSelector timing: the
                    // wrapped copy exists from the moment the rule ran, so
                    // pre-registered extensions apply one-shot to it too
                    // (govuk's contextual rules inside their @media copies).
                    extend_base: self.cur_rule_extend_base,
                };
                let r = self.exec(&block.statements, block.span, parents, &mut child);
                if r.is_ok() {
                    child.flush_rule_block();
                }
                r
            };
            if res.is_ok() {
                body.extend(nested);
            }
            res
        };
        self.pop_scope();
        self.last_child_invisible = saved_last_invisible;
        result?;
        Ok(body)
    }

    /// Evaluate `@media`: resolve the query list (SassScript inside feature
    /// values is evaluated), merge with any enclosing `@media`, run the body
    /// carrying enclosing selectors inside, then emit the at-rule (which bubbles
    /// to the document root). An empty body produces no output.
    pub(super) fn eval_media(
        &mut self,
        query: &'a ox::MediaQueryList<'a>,
        body: &'a ox::SimpleBlock<'a>,
        lines: SrcLines,
        parents: &[String],
        sink: &mut Sink<'_>,
    ) -> Result<(), Error> {
        // Without an enclosing style rule, a bare declaration directly inside a
        // media block is invalid (dart-sass: "expected \"{\".") — only rules and
        // at-rules may appear there. With a style rule, declarations belong to
        // its selector and are allowed — and inside an UNKNOWN at-rule dart's
        // parser context (`_inUnknownAtRule`) legalizes them too
        // (`@utility x { @media (…) { max-width: …; } }`, Tailwind v4).
        if parents.is_empty() && !self.in_unknown_at_rule {
            for stmt in &body.statements {
                if let ox::Statement::Declaration(d) = stmt {
                    // dart reaches this by re-reading the declaration as a
                    // selector and failing to find the `{`, so the caret lands
                    // where that scan gave up — see `Declaration::value_end`.
                    let pos = self.file.pos_at(d.span.end);
                    return Err(self.finalize_error(Error::at("expected \"{\".", pos)));
                }
            }
        }

        let queries = self.resolve_media_queries(query)?;

        // Inside a keyframe block an at-rule nests verbatim: the frame is not
        // a style rule in dart-sass, so there is no bubbling/wrapping.
        if self.in_keyframes && sink.is_rule() {
            let prelude = serialize_media_queries(&queries, self.compressed());
            let out_body = self.eval_at_body(body, &[])?;
            sink.push_item(OutItem::NestedAtRule {
                name: "media".to_string(),
                prelude,
                items: at_body_to_items(out_body),
            });
            return Ok(());
        }

        // Merge with the enclosing media context (dart-sass `_mergeMediaQueries`).
        let merged = if self.media_queries.is_empty() {
            None
        } else {
            match merge_media_query_lists(&self.media_queries, &queries) {
                // Mutually exclusive everywhere — emit nothing.
                Some(m) if m.is_empty() => return Ok(()),
                other => other,
            }
        };

        // Children see the merged queries when mergeable, else just our own.
        let child_queries = merged.clone().unwrap_or_else(|| queries.clone());
        // The emitted node carries the merged queries (when mergeable) and
        // bubbles past the enclosing media; otherwise it stays nested.
        let bubble_out = merged.is_some();
        let node_queries = if bubble_out { &child_queries } else { &queries };
        let prelude = serialize_media_queries(node_queries, self.compressed());

        let enclosing = !self.media_queries.is_empty();
        let saved = std::mem::replace(&mut self.media_queries, child_queries);
        let saved_hoist = std::mem::take(&mut self.media_hoist);
        let own_depth = self.at_rule_ctx.len();
        self.at_rule_ctx.push(AtCtx::Media {
            prelude: prelude.clone(),
        });
        let out_body = self.eval_at_body(body, parents);
        self.at_rule_ctx.pop();
        self.media_queries = saved;
        let mut hoisted = std::mem::replace(&mut self.media_hoist, saved_hoist);
        let out_body = out_body?;

        // Split the body at the hoist markers nested mergeable media rules
        // left behind: each marker interleaves the bubbled rule at its source
        // position, slicing this rule's own children into segments around it
        // (dart-sass#453 keeps source order).
        let mut result: Vec<OutNode> = Vec::new();
        let mut segment: Vec<OutNode> = Vec::new();
        let mut hoist_iter = hoisted.drain(..);
        let flush = |segment: &mut Vec<OutNode>, result: &mut Vec<OutNode>, prelude: &str| {
            if !segment.is_empty() {
                result.push(OutNode::AtRule {
                    name: "media".to_string(),
                    prelude: prelude.to_string(),
                    body: std::mem::take(segment),
                    has_block: true,
                    lines,
                });
            }
        };
        for n in out_body {
            let at_root_target = match &n {
                OutNode::AtRootHoist { target } => Some(*target),
                _ => None,
            };
            if matches!(&n, OutNode::MediaHoist) {
                flush(&mut segment, &mut result, &prelude);
                if let Some(batch) = hoist_iter.next() {
                    result.extend(batch);
                }
            } else if let Some(t) = at_root_target {
                if t == own_depth + 1 {
                    // The batch grafts INTO this rule's own body (dart adds
                    // it to the existing node at its current end): it joins
                    // the current segment in place, splitting nothing.
                    if let Some(b) = self.at_root_hoist.pop_front() {
                        debug_assert_eq!(b.target, t);
                        segment.extend(b.nodes);
                    }
                } else if own_depth == 0 {
                    // A root-bound batch: split around it, placing it just
                    // after this rule's current segment at the root.
                    flush(&mut segment, &mut result, &prelude);
                    if let Some(b) = self.at_root_hoist.pop_front() {
                        debug_assert_eq!(b.target, t);
                        result.extend(b.nodes);
                        result.push(if b.group_end {
                            OutNode::GroupEnd
                        } else {
                            OutNode::AtRootPackTight
                        });
                    }
                } else {
                    // Bound further out: split and pass the marker outward.
                    flush(&mut segment, &mut result, &prelude);
                    result.push(n);
                }
            } else {
                segment.push(n);
            }
        }
        drop(hoist_iter);
        flush(&mut segment, &mut result, &prelude);
        if result.is_empty() {
            return Ok(());
        }

        // A mergeable rule nested in another media bubbles the whole batch
        // out through the enclosing rule (leaving a marker at this source
        // position); otherwise emit in place.
        if bubble_out && enclosing {
            sink.push_at_rule(OutNode::MediaHoist);
            self.media_hoist.push(result);
        } else {
            for n in result {
                sink.push_at_rule(n);
            }
        }
        Ok(())
    }

    /// Resolve a parsed media query list to its final string components,
    /// evaluating SassScript inside feature values.
    pub(super) fn resolve_media_queries(
        &mut self,
        list: &'a ox::MediaQueryList<'a>,
    ) -> Result<Vec<ResolvedQuery>, Error> {
        let mut out = Vec::with_capacity(list.queries.len());
        for q in &list.queries {
            out.push(self.resolve_media_query(q)?);
        }
        // dart-sass re-parses the RESOLVED prelude text (CssMediaQuery
        // .parseList), so interpolation may span query boundaries
        // (`scr#{"een, pri"}nt` splits into two queries). Only a prelude that
        // actually contained interpolation needs the round-trip.
        if self.has_interp(list.span) {
            let text = serialize_media_queries(&out, self.compressed());
            return css_media_parse_list(&text);
        }
        Ok(out)
    }

    fn resolve_media_query(
        &mut self,
        q: &'a ox::MediaQuery<'a>,
    ) -> Result<ResolvedQuery, Error> {
        match q {
            ox::MediaQuery::WithType(t) => {
                let mtype = self.eval_interp_ident(&t.media_type)?;
                let modifier = t.modifier.as_ref().map(|m| m.name.to_string());
                let conditions = match &t.condition {
                    Some(c) => self.resolve_media_condition(&c.condition)?,
                    None => Vec::new(),
                };
                Ok(ResolvedQuery {
                    modifier,
                    mtype: Some(mtype),
                    conditions,
                    conjunction_and: true,
                })
            }
            ox::MediaQuery::ConditionOnly(c) => Ok(ResolvedQuery {
                modifier: None,
                mtype: None,
                conditions: self.resolve_media_condition(c)?,
                conjunction_and: !condition_is_or(c),
            }),
            // A query the typed grammar could not read is spliced text (a
            // Sass interpolation standing for a whole query).
            other => Ok(ResolvedQuery {
                modifier: None,
                mtype: None,
                conditions: vec![self.interpolate(*other.span())?],
                conjunction_and: true,
            }),
        }
    }

    fn resolve_media_condition(
        &mut self,
        c: &'a ox::MediaCondition<'a>,
    ) -> Result<Vec<String>, Error> {
        let mut out = Vec::with_capacity(c.conditions.len());
        for k in &c.conditions {
            out.push(match k {
                ox::MediaConditionKind::MediaInParens(p) => {
                    self.serialize_media_in_parens(p)?
                }
                ox::MediaConditionKind::And(a) => {
                    self.serialize_media_in_parens(&a.media_in_parens)?
                }
                ox::MediaConditionKind::Or(o) => {
                    self.serialize_media_in_parens(&o.media_in_parens)?
                }
                ox::MediaConditionKind::Not(n) => {
                    format!("not {}", self.serialize_media_in_parens(&n.media_in_parens)?)
                }
            });
        }
        Ok(out)
    }

    fn serialize_media_in_parens(
        &mut self,
        p: &'a ox::MediaInParens<'a>,
    ) -> Result<String, Error> {
        match &p.kind {
            ox::MediaInParensKind::MediaFeature(f) => {
                let inner = self.serialize_media_feature(f)?;
                Ok(format!("({inner})"))
            }
            ox::MediaInParensKind::MediaCondition(c) => {
                let parts = self.resolve_media_condition(c)?;
                let sep = if condition_is_or(c) { " or " } else { " and " };
                Ok(format!("({})", parts.join(sep)))
            }
            // Raw interpolation spliced into the query verbatim, e.g.
            // `(a) and #{"(b) and (c)"}`.
            ox::MediaInParensKind::SassInterpolation(i) => self.interpolate(i.span),
            ox::MediaInParensKind::GeneralEnclosed(seq) => {
                Ok(format!("({})", self.interpolate(seq.span)?))
            }
        }
    }

    fn serialize_media_feature(
        &mut self,
        f: &'a ox::MediaFeature<'a>,
    ) -> Result<String, Error> {
        match f {
            // Media-feature names and values serialize in interpolation
            // context: a quoted string unquotes (`("min-width:#{$w}")` emits
            // `(min-width:20px)`), numbers are unchanged.
            ox::MediaFeature::Boolean(b) => self.media_feature_name(&b.name),
            ox::MediaFeature::Plain(pl) => {
                let n = self.media_feature_name(&pl.name)?;
                let v = self.eval_value(&pl.value)?;
                Ok(format!("{n}: {}", interp_checked(&v)?))
            }
            ox::MediaFeature::Range(r) => {
                let a = self.eval_value(&r.left)?.to_css(self.compressed());
                let b = self.eval_value(&r.right)?.to_css(self.compressed());
                Ok(format!("{a} {} {b}", comparison_text(&r.comparison)))
            }
            ox::MediaFeature::RangeInterval(r) => {
                let a = self.eval_value(&r.left)?.to_css(self.compressed());
                let n = self.media_feature_name(&r.name)?;
                let b = self.eval_value(&r.right)?.to_css(self.compressed());
                Ok(format!(
                    "{a} {} {n} {} {b}",
                    comparison_text(&r.left_comparison),
                    comparison_text(&r.right_comparison)
                ))
            }
        }
    }

    /// A media feature's name, which may be an interpolated identifier or a
    /// `$variable` holding one.
    fn media_feature_name(
        &mut self,
        name: &'a ox::MediaFeatureName<'a>,
    ) -> Result<String, Error> {
        match name {
            ox::MediaFeatureName::Ident(i) => self.eval_interp_ident(i),
            ox::MediaFeatureName::SassVariable(v) => {
                let value = self.eval_variable(v.name.name, self.file.pos(v.span))?;
                interp_checked(&value)
            }
            other => self.interpolate(*other.span()),
        }
    }

    /// Evaluate `@supports <condition> { body }`: serialize the structured
    /// condition canonically, run the body (bubbling like any at-rule), and emit
    /// the node — skipping emission entirely when the body produces nothing
    /// (dart-sass drops an empty/invisible `@supports`).
    pub(super) fn eval_supports(
        &mut self,
        condition: &'a ox::SupportsCondition<'a>,
        body: &'a ox::SimpleBlock<'a>,
        lines: SrcLines,
        parents: &[String],
        sink: &mut Sink<'_>,
    ) -> Result<(), Error> {
        let prelude = self.serialize_supports_condition(condition)?;
        let own_depth = self.at_rule_ctx.len();
        self.at_rule_ctx.push(AtCtx::Supports {
            prelude: prelude.clone(),
        });
        let out_body = self.eval_at_body(body, parents);
        self.at_rule_ctx.pop();
        let out_body = out_body?;
        if out_body.is_empty() {
            return Ok(());
        }
        // Split around any escaping @at-root batches (markers), wrapping each
        // segment in its own @supports copy like dart's tree rebuild.
        let mut result: Vec<OutNode> = Vec::new();
        let mut segment: Vec<OutNode> = Vec::new();
        let flush = |segment: &mut Vec<OutNode>, result: &mut Vec<OutNode>| {
            if !segment.is_empty() {
                result.push(OutNode::AtRule {
                    name: "supports".to_string(),
                    prelude: prelude.clone(),
                    body: std::mem::take(segment),
                    has_block: true,
                    lines,
                });
            }
        };
        for n in out_body {
            let at_root_target = match &n {
                OutNode::AtRootHoist { target } => Some(*target),
                _ => None,
            };
            if let Some(t) = at_root_target {
                if t == own_depth + 1 {
                    // Grafts INTO this rule's own body: joins the current
                    // segment in place (dart appends to the existing node).
                    if let Some(b) = self.at_root_hoist.pop_front() {
                        segment.extend(b.nodes);
                    }
                } else if own_depth == 0 {
                    flush(&mut segment, &mut result);
                    if let Some(b) = self.at_root_hoist.pop_front() {
                        result.extend(b.nodes);
                        result.push(if b.group_end {
                            OutNode::GroupEnd
                        } else {
                            OutNode::AtRootPackTight
                        });
                    }
                } else {
                    flush(&mut segment, &mut result);
                    result.push(n);
                }
            } else {
                segment.push(n);
            }
        }
        flush(&mut segment, &mut result);
        for n in result {
            sink.push_at_rule(n);
        }
        Ok(())
    }

    /// Serialize a `@supports` condition to its canonical CSS string
    /// (dart-sass `_visitSupportsCondition`).
    pub(super) fn serialize_supports_condition(
        &mut self,
        condition: &'a ox::SupportsCondition<'a>,
    ) -> Result<String, Error> {
        let mut out = String::new();
        for (i, kind) in condition.conditions.iter().enumerate() {
            match kind {
                ox::SupportsConditionKind::SupportsInParens(p) => {
                    out.push_str(&self.supports_in_parens(p)?)
                }
                ox::SupportsConditionKind::Not(n) => {
                    if i > 0 {
                        out.push(' ');
                    }
                    out.push_str("not ");
                    out.push_str(&self.supports_in_parens(&n.condition)?);
                }
                ox::SupportsConditionKind::And(a) => {
                    out.push_str(" and ");
                    out.push_str(&self.supports_in_parens(&a.condition)?);
                }
                ox::SupportsConditionKind::Or(o) => {
                    out.push_str(" or ");
                    out.push_str(&self.supports_in_parens(&o.condition)?);
                }
            }
        }
        Ok(out)
    }

    /// One operand of a `@supports` condition.
    fn supports_in_parens(
        &mut self,
        p: &'a ox::SupportsInParens<'a>,
    ) -> Result<String, Error> {
        match &p.kind {
            ox::SupportsInParensKind::Feature(f) => self.supports_declaration(&f.decl),
            ox::SupportsInParensKind::SupportsCondition(c) => {
                Ok(format!("({})", self.serialize_supports_condition(c)?))
            }
            ox::SupportsInParensKind::Function(f) => {
                let name = self.interpolate(*f.name.span())?;
                let args = self.interpolate(super::expr::args_span(f))?;
                Ok(format!("{name}({args})"))
            }
            ox::SupportsInParensKind::Selector(sel) => {
                Ok(format!("selector({})", self.interpolate(sel.span)?))
            }
            // A lone interpolation as a condition operand, e.g.
            // `@supports #{"(a: b)"} { ... }`.
            ox::SupportsInParensKind::Interpolation(i) => self.eval_interp_ident(i),
            ox::SupportsInParensKind::GeneralEnclosed(seq) => {
                Ok(format!("({})", self.interpolate(seq.span)?))
            }
        }
    }

    /// A `(<name>: <value>)` feature query.
    fn supports_declaration(
        &mut self,
        decl: &'a ox::Declaration<'a>,
    ) -> Result<String, Error> {
        // dart-sass evaluates BOTH the name and the value with
        // `_inSupportsDeclaration` set, so a calc in the name (`(calc(0): a)`)
        // is also kept unsimplified.
        let custom = decl.name.is_custom_property();
        let saved = self.in_supports_declaration;
        self.in_supports_declaration = true;
        let result = (|| {
            let n = self.eval_interp_ident(&decl.name)?;
            let v = if custom || decl.value_is_raw {
                // A custom-property value is an unquoted string: resolve its
                // interpolation, then apply unquoted-string serialization
                // (`\n` -> space, post-newline spaces dropped), matching
                // dart-sass `_visitUnquotedString`.
                unquoted_string_css(&self.interpolate(values_span(&decl.value))?)
            } else {
                self.eval_values(&decl.value)?.to_css(self.compressed())
            };
            Ok::<_, Error>((n, v))
        })();
        self.in_supports_declaration = saved;
        let (n, v) = result?;
        let sep = if custom { "" } else { " " };
        Ok(format!("({n}:{sep}{v})"))
    }

    /// Serialize a plain-CSS `@import` argument: the URL followed by its
    /// canonical modifiers. A `supports(<query>)` re-serializes its parsed
    /// condition (a declaration's own parens double as the call parens); the
    /// terminal media query list joins with `, `.
    pub(super) fn serialize_css_import(
        &mut self,
        prelude: &'a ox::ImportPrelude<'a>,
    ) -> Result<String, Error> {
        let mut out = self.interpolate(*prelude.href.span())?;
        if let Some(layer) = &prelude.layer {
            out.push(' ');
            out.push_str(&self.interpolate(*layer.span())?);
        }
        if let Some(supports) = &prelude.supports {
            out.push(' ');
            // The condition is an *expression* part of the modifiers
            // interpolation in dart-sass (a `SupportsExpression` whose value is
            // an unquoted string), so its serialized text gets the
            // unquoted-string newline collapse.
            match &supports.kind {
                ox::ImportPreludeSupportsKind::Declaration(d) => {
                    let inner = unquoted_string_css(&self.supports_declaration(d)?);
                    out.push_str(&format!("supports{inner}"));
                }
                ox::ImportPreludeSupportsKind::SupportsCondition(c) => {
                    let inner =
                        unquoted_string_css(&self.serialize_supports_condition(c)?);
                    out.push_str(&format!("supports({inner})"));
                }
            }
        }
        if let Some(media) = &prelude.media {
            out.push(' ');
            let queries = self.resolve_media_queries(media)?;
            out.push_str(&serialize_media_queries(&queries, self.compressed()));
        }
        // Modifiers the typed grammar could not read are kept as written.
        if let Some(modifiers) = &prelude.modifiers {
            out.push(' ');
            out.push_str(&self.interpolate(modifiers.span)?);
        }
        Ok(out)
    }

    /// Evaluate `@keyframes`. The frame selectors are keyframe selectors, not
    /// CSS selectors: no `&`/parent resolution. We run the body with the parent
    /// context reset to root (empty parents), so frame blocks emit verbatim.
    /// The whole node bubbles to the document root like any other at-rule.
    pub(super) fn eval_keyframes(
        &mut self,
        name: &str,
        prelude: Option<Span>,
        body: &'a ox::SimpleBlock<'a>,
        lines: SrcLines,
        sink: &mut Sink<'_>,
    ) -> Result<(), Error> {
        // A style rule nested inside a keyframe block is invalid; each frame
        // (a top-level rule in the body) may only hold declarations.
        for stmt in &body.statements {
            let frame = match stmt {
                ox::Statement::QualifiedRule(r) => &r.block,
                ox::Statement::KeyframeBlock(k) => &k.block,
                _ => continue,
            };
            for inner in &frame.statements {
                if matches!(inner, ox::Statement::QualifiedRule(_)) {
                    let e = Error::unpositioned(
                        "Style rules may not be used within keyframe blocks.",
                    );
                    let span = *inner.span();
                    return Err(self.finalize_error(e.or_at(
                        self.file.pos(span),
                        span.end.saturating_sub(span.start),
                    )));
                }
            }
        }
        let prelude = self.prelude_text(prelude)?;
        let saved_kf = std::mem::replace(&mut self.in_keyframes, true);
        let own_depth = self.at_rule_ctx.len();
        self.at_rule_ctx.push(AtCtx::Keyframes {
            name: name.to_string(),
            prelude: prelude.clone(),
        });
        let out_body = self.eval_at_body(body, &[]);
        self.at_rule_ctx.pop();
        self.in_keyframes = saved_kf;
        let out_body = out_body?;
        // Pull any escaping @at-root batches out (the keyframes shell stays,
        // even empty: `@keyframes a {}` + the hoisted rules after it).
        let mut shell: Vec<OutNode> = Vec::new();
        let mut after: Vec<OutNode> = Vec::new();
        for n in out_body {
            let at_root_target = match &n {
                OutNode::AtRootHoist { target } => Some(*target),
                _ => None,
            };
            if let Some(t) = at_root_target {
                if t == own_depth + 1 {
                    if let Some(b) = self.at_root_hoist.pop_front() {
                        shell.extend(b.nodes);
                    }
                } else if own_depth == 0 {
                    if let Some(b) = self.at_root_hoist.pop_front() {
                        after.extend(b.nodes);
                        after.push(if b.group_end {
                            OutNode::GroupEnd
                        } else {
                            OutNode::AtRootPackTight
                        });
                    }
                } else {
                    after.push(n);
                }
            } else {
                shell.push(n);
            }
        }
        sink.push_at_rule(OutNode::AtRule {
            name: name.to_string(),
            prelude,
            body: shell,
            has_block: true,
            lines,
        });
        for n in after {
            sink.push_at_rule(n);
        }
        Ok(())
    }

    /// Evaluate `@at-root`: hoist the body's output to the document root. The
    /// parent-selector context is KEPT — dart resolves `&` against the
    /// enclosing rule but disables the implicit parent join
    /// (`implicitParent: !_atRootExcludingStyleRule`), so `@at-root & {…}`
    /// re-emits the parent at the root while `@at-root .x {…}` stays `.x`.
    /// `@at-root <selector> { … }`: the prelude is the rule's own selector and
    /// the whole rule is lifted to the document root, so it resolves against no
    /// parent (dart-sass `_atRootExcludingStyleRule` with the selector applied).
    pub(super) fn eval_at_root_selector(
        &mut self,
        selector: &'a ox::SelectorList<'a>,
        block: &'a ox::SimpleBlock<'a>,
        parents: &[String],
        sink: &mut Sink<'_>,
    ) -> Result<(), Error> {
        // Only the IMPLICIT parent join is disabled: an explicit `&` still
        // resolves against the enclosing rule, so its selectors come along.
        let saved = std::mem::replace(&mut self.at_root_excluding_style_rule, true);
        let result = self.eval_style_rule_at(selector.span, block, parents, sink);
        self.at_root_excluding_style_rule = saved;
        result
    }

    /// The optional query is accepted but not yet honoured (the common
    /// no-query case is supported).
    pub(super) fn eval_at_root(
        &mut self,
        query: Option<Span>,
        body: &'a ox::SimpleBlock<'a>,
        parents: &[String],
        sink: &mut Sink<'_>,
    ) -> Result<(), Error> {
        let query_text = match query {
            Some(span) => Some(self.interpolate(span)?),
            None => None,
        };
        let q = AtRootQuery::parse(query_text.as_deref());
        // Which enclosing at-rule layers the query keeps vs excludes (dart
        // `AtRootQuery.excludes`). The topmost excluded layer is the graft
        // target (dart `_trimIncluded`): kept layers ABOVE it stay in place
        // (the batch re-enters inside the innermost of them), kept layers
        // BELOW it are copied around the hoisted body.
        let excluded: Vec<bool> = self
            .at_rule_ctx
            .iter()
            .map(|c| q.excludes_name(c.query_name()))
            .collect();
        let first_excluded = excluded.iter().position(|&e| e);
        let any_excluded_layer = first_excluded.is_some();
        // `(with: all)`-style queries that exclude nothing run in place.
        if !any_excluded_layer && !q.excludes_style_rules() {
            return self.exec(&body.statements, body.span, parents, sink);
        }

        self.push_scope(false);
        let saved = self.at_root_excluding_style_rule;
        self.at_root_excluding_style_rule = q.excludes_style_rules();
        // An excluded media layer also stops feeding the body's own @media
        // merging; an excluded keyframes layer drops the keyframe context.
        let saved_media = if q.excludes_name("media") {
            Some(std::mem::take(&mut self.media_queries))
        } else {
            None
        };
        let saved_kf = if q.excludes_name("keyframes") {
            Some(std::mem::replace(&mut self.in_keyframes, false))
        } else {
            None
        };
        let mut out: Vec<OutNode> = Vec::new();
        let res = {
            let mut child = Sink::AtRoot {
                body: &mut out,
                group_ends: true,
            };
            self.exec(&body.statements, body.span, parents, &mut child)
        };
        self.at_root_excluding_style_rule = saved;
        if let Some(m) = saved_media {
            self.media_queries = m;
        }
        if let Some(k) = saved_kf {
            self.in_keyframes = k;
        }
        self.pop_scope();
        res?;

        // A declaration inside `@at-root` that no style rule wraps lands at
        // the document root, which dart rejects ("Declarations may only be
        // used within style rules." — issue_1585's `@at-root { @content }`).
        if (q.excludes_style_rules() || parents.is_empty())
            && out.iter().any(|n| matches!(n, OutNode::AtDecl { .. }))
        {
            return Err(Error::unpositioned(
                "Declarations may only be used within style rules.",
            ));
        }

        // When the query KEEPS style rules, bare declarations re-wrap in the
        // enclosing selectors (dart's included CssStyleRule copy):
        // `a { @at-root (without: media) { b: c } }` emits `a { b: c }`.
        let out = if !q.excludes_style_rules() && !parents.is_empty() {
            let mut wrapped: Vec<OutNode> = Vec::new();
            let mut decls: Vec<OutItem> = Vec::new();
            let flush = |decls: &mut Vec<OutItem>, wrapped: &mut Vec<OutNode>| {
                if !decls.is_empty() {
                    wrapped.push(OutNode::plain_rule(
                        parents.to_vec(),
                        std::mem::take(decls),
                        SrcLines::default(),
                    ));
                    // The re-wrapped copy is a completed style rule too.
                    wrapped.push(OutNode::GroupEnd);
                }
            };
            for n in out {
                match n {
                    OutNode::AtDecl {
                        prop,
                        value,
                        important,
                        custom,
                        lines,
                        value_span,
                    } => decls.push(OutItem::Decl {
                        prop,
                        value,
                        important,
                        custom,
                        lines,
                        value_span,
                    }),
                    other => {
                        flush(&mut decls, &mut wrapped);
                        wrapped.push(other);
                    }
                }
            }
            flush(&mut decls, &mut wrapped);
            wrapped
        } else {
            out
        };

        // Hoisted root-level nodes separate with a blank line only after a
        // completed style rule (dart's isGroupEnd: `#inc {…}` → blank →
        // `@supports`, but `@supports {…}` → `@foo` packs tight). The
        // group-end sentinels were pushed by `Sink::AtRoot`'s emit — one per
        // rule completed at the body's own level; rules flattened out of a
        // WRAPPER rule (`@at-root .slim & { &__a{} &__b{} }`) share one batch
        // and pack tight, exactly like dart's `_styleRule == null` gate. An
        // interior sentinel becomes the one blank line; a trailing one stays
        // (the graft's next sibling separates against it).
        let mut spaced: Vec<OutNode> = materialize_interior_group_ends(out);
        // A TRAILING sentinel has done its job (it only gated the interior
        // separators above): pop it so it can't ride into the escape path and
        // conjure an empty resume shell (`@media print {}`), or double the
        // top-level `push_group` default blank-after-rule.
        while matches!(spaced.last(), Some(OutNode::GroupEnd)) {
            spaced.pop();
        }
        if spaced
            .iter()
            .all(|n| matches!(n, OutNode::Blank | OutNode::GroupEnd | OutNode::AtRootPackTight))
        {
            return Ok(());
        }
        if let Some(te) = first_excluded {
            // Escape the excluded at-rules: re-wrap the body in the kept
            // layers BELOW the graft target (innermost-last) and leave a
            // marker; each enclosing layer below the target splits around it
            // and passes it outward until the target layer consumes it.
            let mut batch = spaced;
            for (i, layer) in self.at_rule_ctx.iter().enumerate().rev() {
                if i > te && !excluded[i] {
                    batch = vec![layer.wrap(batch)];
                }
            }
            self.at_root_hoist.push_back(AtRootBatch {
                target: te,
                group_end: parents.is_empty(),
                nodes: batch,
            });
            sink.push_at_rule(OutNode::AtRootHoist { target: te });
        } else {
            // No layer escaped: the body stays inside the enclosing at-rules
            // (only the style-rule join was disabled), so emit in place.
            // When the hoisted chunk ends in a style rule AND an enclosing style
            // rule resumes after it, dart separates the resumed parent with one
            // blank line (isGroupEnd). Leave a group-end marker that
            // `emit_style_rule` materializes into that blank (a trailing marker —
            // nothing resumes — stays a no-op, so no spurious trailing blank).
            let ends_in_rule = matches!(spaced.last(), Some(OutNode::Rule { .. }));
            for node in spaced {
                // Push inter-rule separators as GROUP-END markers, not literal
                // blanks, so the group machinery inserts EXACTLY one blank: at the
                // document root `push_group` turns the marker into one separator
                // (a literal blank would be double-counted into three), and inside
                // a resumed rule `emit_style_rule` materializes it into one.
                if matches!(node, OutNode::Blank) {
                    sink.push_at_rule(OutNode::GroupEnd);
                } else {
                    sink.push_at_rule(node);
                }
            }
            if ends_in_rule && !parents.is_empty() {
                sink.push_at_rule(OutNode::GroupEnd);
            }
        }
        Ok(())
    }
}

/// Whether a media condition's operands are joined with `or` rather than
/// `and`. dart-sass models a query as one conjunction, so the first `or`
/// decides for the whole run.
fn condition_is_or(c: &ox::MediaCondition<'_>) -> bool {
    c.conditions
        .iter()
        .any(|k| matches!(k, ox::MediaConditionKind::Or(_)))
}

/// The source spelling of a media-feature range comparison.
fn comparison_text(c: &ox::MediaFeatureComparison) -> &'static str {
    match c.kind {
        ox::MediaFeatureComparisonKind::LessThan => "<",
        ox::MediaFeatureComparisonKind::LessThanOrEqual => "<=",
        ox::MediaFeatureComparisonKind::GreaterThan => ">",
        ox::MediaFeatureComparisonKind::GreaterThanOrEqual => ">=",
        ox::MediaFeatureComparisonKind::Equal => "=",
    }
}

/// Re-serialize an at-rule prelude the way dart-sass does: drop comments and
/// collapse each whitespace run to a single space.
fn collapse_prelude(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let b = text.as_bytes();
    let mut i = 0usize;
    let mut pending_space = false;
    while i < b.len() {
        // A `/* … */` comment is trivia in a prelude, not text.
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            match text[i + 2..].find("*/") {
                Some(rel) => {
                    i += 2 + rel + 2;
                    pending_space = true;
                    continue;
                }
                None => break,
            }
        }
        let c = text[i..].chars().next().unwrap_or(' ');
        if c.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        }
        i += c.len_utf8();
    }
    out
}
