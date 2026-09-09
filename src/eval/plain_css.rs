use super::*;

impl<'a> Evaluator<'a> {
    /// Emit a plain-CSS (`.css`) module's statements, preserving nesting (no
    /// Sass flattening), keeping `&` parent references literal, and resolving
    /// only `#{…}` interpolation. The parser read the file under
    /// [`Syntax::Css`](oxc_css_parser::Syntax), so the Sass-only constructs are
    /// already rejected and the remaining statements are plain CSS.
    pub(super) fn exec_css(
        &mut self,
        block: &'a ox::SimpleBlock<'a>,
        parents: &[String],
        sink: &mut Sink<'_>,
    ) -> Result<(), Error> {
        let saved = std::mem::replace(&mut self.in_plain_css, true);
        let result = self.exec_css_inner(block, parents, sink);
        self.in_plain_css = saved;
        result
    }

    fn exec_css_inner(
        &mut self,
        block: &'a ox::SimpleBlock<'a>,
        parents: &[String],
        sink: &mut Sink<'_>,
    ) -> Result<(), Error> {
        // When the plain-CSS sheet is imported inside a style rule, top-level
        // rules whose selector contains a parent reference `&` keep native
        // CSS-nesting semantics: they are emitted VERBATIM as nested children
        // of one leading parent-selector shell (dart `nestWithin` with
        // `preserveParentSelectors`), while `&`-less rules get the descendant
        // join below.
        if !parents.is_empty() {
            let mut preserved: Vec<OutItem> = Vec::new();
            for stmt in &block.statements {
                if let ox::Statement::QualifiedRule(r) = stmt {
                    let (own, _) = self.css_selectors(r.selector.span, true)?;
                    if own.iter().any(|s| part_has_parent_ref(s)) {
                        let inner = self.css_body(&r.block)?;
                        if !inner.is_empty() {
                            preserved.push(OutItem::NestedRule {
                                selectors: own,
                                items: inner,
                            });
                        }
                    }
                }
            }
            if !preserved.is_empty() {
                sink.push_at_rule(OutNode::plain_rule(
                    parents.to_vec(),
                    preserved,
                    SrcLines::default(),
                ));
            }
        }

        for item in interleave(self.file, &block.statements, block.span) {
            match item {
                Item::Comment(c) => {
                    let (text, lines) = self.css_comment(c)?;
                    sink.push_at_rule(OutNode::Comment(text, lines));
                }
                Item::Stmt(ox::Statement::QualifiedRule(r)) => {
                    // When the plain-CSS sheet is imported inside a style rule,
                    // its outermost rules nest under the Sass parent
                    // (descendant join); inner nesting stays native (dart-sass
                    // `nestWithin` with `preserveParentSelectors`). The sheet's
                    // own top level always rejects leading combinators — also
                    // when merged under a Sass parent (dart checks in the merge
                    // branch).
                    let (own, own_lbs) = self.css_selectors(r.selector.span, true)?;
                    // A `&`-bearing rule was already emitted in the leading
                    // parent shell above.
                    if !parents.is_empty() && own.iter().any(|s| part_has_parent_ref(s)) {
                        continue;
                    }
                    // Line structure carries through only for the sheet's own
                    // top level; the descendant join under a Sass parent
                    // re-shapes the list.
                    let linebreaks = if parents.is_empty() {
                        own_lbs
                    } else {
                        Vec::new()
                    };
                    let selectors: Vec<String> = if parents.is_empty() {
                        own
                    } else {
                        parents
                            .iter()
                            .flat_map(|p| own.iter().map(move |s| format!("{p} {s}")))
                            .collect()
                    };
                    let (items, bubbled) = self.css_rule_children(&r.block, &selectors)?;
                    // A childless rule is invisible (dart-sass skips it when
                    // serializing) — e.g. when its whole body bubbled out.
                    if !items.is_empty() {
                        let lines = self.css_rule_lines(r);
                        sink.push_at_rule(OutNode::Rule {
                            selectors: RuleSelectors::Raw(selectors),
                            linebreaks,
                            items,
                            lines,
                            extend_base: usize::MAX,
                        });
                    }
                    for node in bubbled {
                        sink.push_at_rule(node);
                    }
                }
                Item::Stmt(ox::Statement::AtRule(at)) => {
                    let (name, prelude) = self.css_at_prelude(at)?;
                    let lines = self.css_at_lines(at);
                    match &at.block {
                        // A plain CSS file never inlines an `@import`; every
                        // entry is emitted verbatim, matching dart-sass loading
                        // a `.css` stylesheet.
                        None => sink.push_at_rule(OutNode::AtRule {
                            name,
                            prelude,
                            body: Vec::new(),
                            has_block: false,
                            lines,
                        }),
                        Some(b) => {
                            let out_body = self.css_at_body(b)?;
                            if !out_body.is_empty() {
                                sink.push_at_rule(OutNode::AtRule {
                                    name,
                                    prelude,
                                    body: out_body,
                                    has_block: true,
                                    lines,
                                });
                            }
                        }
                    }
                }
                // A declaration at the top level of a plain-CSS sheet is
                // rejected by the parser, and nothing else the CSS grammar
                // produces reaches the document root.
                Item::Stmt(_) => {}
            }
        }
        Ok(())
    }

    /// Build the body of a top-level plain-CSS at-rule: style rules (with their
    /// own first-level bubbling), bare declarations, comments, and nested
    /// at-rules.
    fn css_at_body(
        &mut self,
        block: &'a ox::SimpleBlock<'a>,
    ) -> Result<Vec<OutNode>, Error> {
        let mut out: Vec<OutNode> = Vec::new();
        for item in interleave(self.file, &block.statements, block.span) {
            match item {
                Item::Comment(c) => {
                    let (text, lines) = self.css_comment(c)?;
                    out.push(OutNode::Comment(text, lines));
                }
                Item::Stmt(ox::Statement::QualifiedRule(r)) => {
                    let (selectors, linebreaks) = self.css_selectors(r.selector.span, false)?;
                    let (items, bubbled) = self.css_rule_children(&r.block, &selectors)?;
                    if !items.is_empty() {
                        let lines = self.css_rule_lines(r);
                        out.push(OutNode::Rule {
                            selectors: RuleSelectors::Raw(selectors),
                            linebreaks,
                            items,
                            lines,
                            extend_base: usize::MAX,
                        });
                    }
                    out.extend(bubbled);
                }
                Item::Stmt(ox::Statement::Declaration(d)) => {
                    let (prop, value, custom, lines, value_span) = self.css_decl(d)?;
                    out.push(OutNode::AtDecl {
                        prop,
                        value,
                        important: d.important.is_some(),
                        custom,
                        lines,
                        value_span,
                    });
                }
                Item::Stmt(ox::Statement::AtRule(at)) => {
                    let (name, prelude) = self.css_at_prelude(at)?;
                    let lines = self.css_at_lines(at);
                    match &at.block {
                        None => out.push(OutNode::AtRule {
                            name,
                            prelude,
                            body: Vec::new(),
                            has_block: false,
                            lines,
                        }),
                        Some(b) => {
                            let inner = self.css_at_body(b)?;
                            if !inner.is_empty() {
                                out.push(OutNode::AtRule {
                                    name,
                                    prelude,
                                    body: inner,
                                    has_block: true,
                                    lines,
                                });
                            }
                        }
                    }
                }
                Item::Stmt(_) => {}
            }
        }
        Ok(out)
    }

    /// Build the children of a *top-level* plain-CSS style rule: declarations
    /// and nested rules stay in the block; a block at-rule (`@media` etc.)
    /// bubbles out wrapping a copy of the parent rule (dart-sass's standard
    /// at-rule bubbling — `a {@media b {c: d}}` → `@media b { a { c: d } }`).
    /// Deeper levels never bubble (see [`Evaluator::css_body`]).
    #[allow(clippy::type_complexity)]
    fn css_rule_children(
        &mut self,
        block: &'a ox::SimpleBlock<'a>,
        parent_selectors: &[String],
    ) -> Result<(Vec<OutItem>, Vec<OutNode>), Error> {
        let mut items = Vec::new();
        let mut bubbled: Vec<OutNode> = Vec::new();
        for item in interleave(self.file, &block.statements, block.span) {
            match item {
                // Only a *block* at-rule bubbles; a childless one stays put.
                Item::Stmt(ox::Statement::AtRule(at)) if at.block.is_some() => {
                    let (name, prelude) = self.css_at_prelude(at)?;
                    let inner = self.css_body(at.block.as_ref().expect("checked"))?;
                    if !inner.is_empty() {
                        bubbled.push(OutNode::AtRule {
                            name,
                            prelude,
                            body: vec![OutNode::plain_rule(
                                parent_selectors.to_vec(),
                                inner,
                                SrcLines::default(),
                            )],
                            has_block: true,
                            lines: SrcLines::default(),
                        });
                    }
                }
                other => self.css_body_item(other, &mut items)?,
            }
        }
        Ok((items, bubbled))
    }

    /// Resolve a plain-CSS selector to its comma-separated parts, keeping `&`
    /// and combinators verbatim (no parent resolution), and rejecting the
    /// Sass-only selector forms that plain CSS forbids. The parts come back
    /// re-serialized the way dart's parser emits them (identifier attribute
    /// values lose their quotes, whitespace collapses), along with the
    /// source's per-complex line-break flags (`a,\nb` keeps its lines).
    fn css_selectors(
        &mut self,
        sel: Span,
        top_level: bool,
    ) -> Result<(Vec<String>, Vec<bool>), Error> {
        let s = self.interpolate(sel)?;
        let parts: Vec<String> = split_commas(&s)
            .into_iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        for p in &parts {
            validate_plain_css_selector(p, top_level)?;
        }
        let normalized: Vec<String> = parts.iter().map(|p| normalize_selector(p)).collect();
        let linebreaks = if s.contains('\n') {
            comma_linebreaks(&s, false)
        } else {
            Vec::new()
        };
        Ok((normalized, linebreaks))
    }

    /// Build a plain-CSS rule body below the first nesting level: declarations
    /// and nested style rules with nesting preserved (`OutItem::NestedRule`),
    /// and block at-rules kept in place (`OutItem::NestedAtRule`) — dart-sass
    /// `_hasCssNesting` skips bubbling once nesting is already native.
    fn css_body(&mut self, block: &'a ox::SimpleBlock<'a>) -> Result<Vec<OutItem>, Error> {
        let mut items = Vec::new();
        for item in interleave(self.file, &block.statements, block.span) {
            self.css_body_item(item, &mut items)?;
        }
        Ok(items)
    }

    /// Process one plain-CSS block entry into rule-body items (the shared body
    /// of [`Evaluator::css_body`] and the non-bubbling arm of
    /// [`Evaluator::css_rule_children`]).
    fn css_body_item(
        &mut self,
        item: Item<'a, 'a>,
        items: &mut Vec<OutItem>,
    ) -> Result<(), Error> {
        match item {
            Item::Comment(c) => {
                let (text, lines) = self.css_comment(c)?;
                items.push(OutItem::Comment(text, lines));
            }
            Item::Stmt(ox::Statement::Declaration(d)) => {
                let (prop, value, custom, lines, value_span) = self.css_decl(d)?;
                items.push(OutItem::Decl {
                    prop,
                    value,
                    important: d.important.is_some(),
                    custom,
                    lines,
                    value_span,
                });
            }
            Item::Stmt(ox::Statement::QualifiedRule(r)) => {
                let (selectors, _) = self.css_selectors(r.selector.span, false)?;
                let inner = self.css_body(&r.block)?;
                // An (recursively) empty nested rule is invisible (dart-sass
                // skips childless rules when serializing).
                if !inner.is_empty() {
                    items.push(OutItem::NestedRule {
                        selectors,
                        items: inner,
                    });
                }
            }
            Item::Stmt(ox::Statement::AtRule(at)) => {
                let (name, prelude) = self.css_at_prelude(at)?;
                match &at.block {
                    // A nested `@import` inside a plain-CSS rule is preserved
                    // verbatim, like a top-level one (see `exec_css`).
                    None => {
                        let lines = self.css_at_lines(at);
                        items.push(OutItem::ChildlessAtRule {
                            name,
                            prelude,
                            lines,
                        });
                    }
                    Some(b) => {
                        let inner = self.css_body(b)?;
                        if !inner.is_empty() {
                            items.push(OutItem::NestedAtRule {
                                name,
                                prelude,
                                items: inner,
                            });
                        }
                    }
                }
            }
            Item::Stmt(_) => {}
        }
        Ok(())
    }

    /// A plain-CSS declaration's property, value, and line metadata.
    ///
    /// Plain CSS has no variables, so the value node is always the value's own
    /// text (dart's `_expressionNode` fallback).
    #[allow(clippy::type_complexity)]
    fn css_decl(
        &mut self,
        d: &'a ox::Declaration<'a>,
    ) -> Result<(String, String, bool, SrcLines, VarSpan), Error> {
        let prop = trim_owned(self.decl_name(d)?);
        let custom = d.name.is_custom_property() || d.value_is_raw;
        let value_span = super::value_read::values_span(&d.value);
        let value = if custom {
            self.interpolate(value_span)?
        } else {
            self.eval_values(&d.value)?.to_css(false)
        };
        let pos = self.file.pos(d.span);
        let value_pos = self.file.pos(value_span);
        let span = self.span_at(value_pos);
        let lines = self.stamp(SrcLines {
            file: 0,
            start: pos.line as u32,
            end: self.file.line_at(d.span.end),
            col: 0,
            start_col: pos.col as u32,
            map_file: 0,
            map_line: 0,
        });
        Ok((prop, value, d.name.is_custom_property(), lines, span))
    }

    /// A plain-CSS at-rule's name and canonical prelude text. `@media` and
    /// `@supports` re-serialize through their typed forms; everything else is
    /// its prelude text with interpolation resolved.
    fn css_at_prelude(
        &mut self,
        at: &'a ox::AtRule<'a>,
    ) -> Result<(String, String), Error> {
        let name = at.name.name.to_string();
        let prelude = match &at.prelude {
            Some(ox::AtRulePrelude::Media(q)) => {
                let queries = self.resolve_media_queries(q)?;
                serialize_media_queries(&queries, self.compressed())
            }
            Some(ox::AtRulePrelude::Supports(c)) => self.serialize_supports_condition(c)?,
            Some(ox::AtRulePrelude::Import(imp)) => self.serialize_css_import(imp)?,
            other => self.prelude_text(other.as_ref().map(|p| *p.span()))?,
        };
        Ok((name, prelude))
    }

    /// A plain-CSS at-rule's source lines.
    fn css_at_lines(&mut self, at: &'a ox::AtRule<'a>) -> SrcLines {
        let pos = self.file.pos(at.span);
        let (start, end) = match &at.block {
            Some(b) => (
                self.file.line_at(b.span.start),
                self.file.line_at(b.span.end),
            ),
            None => (pos.line as u32, self.file.line_at(at.span.end)),
        };
        self.stamp(SrcLines {
            file: 0,
            start,
            end,
            col: 0,
            start_col: pos.col as u32,
            map_file: 0,
            map_line: 0,
        })
    }

    /// A plain-CSS style rule's source lines.
    fn css_rule_lines(&mut self, r: &'a ox::QualifiedRule<'a>) -> SrcLines {
        self.stamp(SrcLines {
            file: 0,
            start: self.file.line_at(r.block.span.start),
            end: self.file.line_at(r.block.span.end),
            col: 0,
            start_col: self.file.pos(r.selector.span).col as u32,
            map_file: 0,
            map_line: 0,
        })
    }

    /// A plain-CSS loud comment's text and source lines.
    fn css_comment(
        &mut self,
        c: &oxc_css_parser::token::Comment<'a>,
    ) -> Result<(String, SrcLines), Error> {
        let body = Span {
            start: c.span.start + 2,
            end: c.span.start + 2 + c.content.len(),
        };
        let text = self.interpolate(body)?;
        let pos = self.file.pos(c.span);
        let lines = self.stamp(SrcLines {
            file: 0,
            start: pos.line as u32,
            end: self.file.line_at(c.span.end),
            col: pos.col as u32,
            start_col: pos.col as u32,
            map_file: 0,
            map_line: 0,
        });
        Ok((text, lines))
    }
}
