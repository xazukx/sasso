//! The staged compile API: parse -> (inspect/modify) -> evaluate ->
//! (inspect/modify) -> emit, plus `Options::record_variable_values`.
//!
//! These gate the *embedder-facing* contract that the intermediate forms are
//! reachable and mutable, and that routing the one-shot `compile` through the
//! same three steps still produces byte-identical CSS.

use sasso::ast::{Expr, Stmt, TplPiece};
use sasso::selector::{Combinator, Complex, ComplexComponent, Compound, Simple};
use sasso::value::Value;
use sasso::{compile, Compiler, Options, OutNode, OutputStyle, RuleSelectors};

/// The staged pipeline reproduces `compile` byte-for-byte.
#[test]
fn staged_pipeline_matches_compile() {
    let src = "\
$pad: 4px;
.a { padding: $pad; &:hover { color: red; } }
@media (min-width: 10px) { .b { color: blue; } }
%p { color: green; }
.c { @extend %p; }
";
    for style in [OutputStyle::Expanded, OutputStyle::Compressed] {
        let options = Options::default().with_style(style);
        let compiler = Compiler::new(src, &options);
        let sheet = compiler.parse().unwrap();
        let run = compiler.evaluate(&sheet).unwrap();
        assert_eq!(compiler.emit(&run.nodes), compile(src, &options).unwrap());
    }
}

/// A parse error surfaces from `parse`, not from `evaluate`.
#[test]
fn staged_parse_reports_syntax_errors() {
    let options = Options::default();
    let err = Compiler::new("a { color: ", &options)
        .parse()
        .expect_err("expected a parse error");
    assert!(err.line > 0, "expected a positioned parse error: {err}");
}

/// The declaration-placement check runs on the sheet handed to `evaluate`, so a
/// caller-built violation is still rejected.
#[test]
fn staged_evaluate_validates_modified_sheet() {
    let options = Options::default();
    let compiler = Compiler::new("@mixin m { a { b: c; } }", &options);
    let mut sheet = compiler.parse().unwrap();
    // Move the `@mixin` inside an `@if`, which Sass forbids.
    let mixin = sheet.stmts.remove(0);
    sheet.stmts.push(Stmt::If(vec![sasso::ast::IfBranch {
        cond: Some(Expr::Bool(true)),
        body: vec![mixin],
    }]));
    let err = compiler
        .evaluate(&sheet)
        .err()
        .expect("expected a validation error");
    assert!(
        err.message
            .contains("Mixins may not be declared in control directives."),
        "{err}"
    );
}

/// The parsed stylesheet is modifiable before evaluation: dropping a statement
/// drops its output, and rewriting a selector template changes the CSS.
#[test]
fn stylesheet_is_modifiable_before_evaluation() {
    let options = Options::default();
    let compiler = Compiler::new("a { color: red; }\nb { color: blue; }", &options);
    let mut sheet = compiler.parse().unwrap();
    assert_eq!(sheet.stmts.len(), 2);

    // Rewrite the first rule's selector, drop the second rule entirely.
    if let Stmt::Rule(rule) = &mut sheet.stmts[0] {
        rule.selector = vec![TplPiece::Lit(".renamed".to_string())];
    } else {
        panic!("expected a style rule");
    }
    sheet.stmts.truncate(1);

    let run = compiler.evaluate(&sheet).unwrap();
    assert_eq!(compiler.emit(&run.nodes), ".renamed {\n  color: red;\n}");
}

/// The evaluated output tree is modifiable before emit — both its raw selector
/// strings and its declarations.
#[test]
fn out_nodes_are_modifiable_before_emit() {
    let options = Options::default();
    let compiler = Compiler::new("a { color: red; }", &options);
    let sheet = compiler.parse().unwrap();
    let mut run = compiler.evaluate(&sheet).unwrap();

    for node in &mut run.nodes {
        if let OutNode::Rule { selectors, items, .. } = node {
            *selectors = RuleSelectors::Raw(
                selectors
                    .to_strings()
                    .iter()
                    .map(|s| format!(".theme {s}"))
                    .collect(),
            );
            items.push(sasso::OutItem::Decl {
                prop: "border".to_string(),
                value: "0".to_string(),
                important: false,
                custom: false,
                lines: sasso::ast::SrcLines::default(),
                value_span: sasso::VarSpan::default(),
            });
        }
    }

    assert_eq!(
        compiler.emit(&run.nodes),
        ".theme a {\n  color: red;\n  border: 0;\n}"
    );
}

/// A rule's selectors can be swapped for the typed `Complex` model, which emit
/// renders through the same path the `@extend` engine uses.
#[test]
fn out_node_selectors_accept_the_typed_model() {
    let options = Options::default();
    let compiler = Compiler::new("a { color: red; }", &options);
    let sheet = compiler.parse().unwrap();
    let mut run = compiler.evaluate(&sheet).unwrap();

    // `.parent > a`, built by hand rather than parsed.
    let parsed = sasso::selector::parse_list("a").expect("selector parses");
    let mut complex = parsed[0].clone();
    complex.components.insert(
        0,
        ComplexComponent {
            combinators: Vec::new(),
            compound: Compound {
                simples: vec![Simple::Class("parent".to_string())],
            },
        },
    );
    complex.components[1].combinators = vec![Combinator::Child];
    assert_eq!(complex.render(), ".parent > a");

    for node in &mut run.nodes {
        if let OutNode::Rule { selectors, .. } = node {
            *selectors = RuleSelectors::Parsed(vec![complex.clone()].into());
        }
    }
    assert_eq!(compiler.emit(&run.nodes), ".parent > a {\n  color: red;\n}");

    // And the typed list renders back to strings for inspection.
    let round_trip: Vec<Complex> = sasso::selector::parse_list(".parent > a").unwrap();
    assert_eq!(round_trip[0].render(), ".parent > a");
}

/// One evaluated tree can be emitted repeatedly under different options.
#[test]
fn one_tree_emits_under_several_styles() {
    let src = "a { color: red; }";
    let expanded = Options::default();
    let compiler = Compiler::new(src, &expanded);
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();

    let compressed = Options::default().with_style(OutputStyle::Compressed);
    assert_eq!(compiler.emit(&run.nodes), "a {\n  color: red;\n}");
    assert_eq!(
        Compiler::new(src, &compressed).emit(&run.nodes),
        compile(src, &compressed).unwrap()
    );
}

/// Nothing is recorded unless the option is set.
#[test]
fn variable_values_are_not_recorded_by_default() {
    let options = Options::default();
    let compiler = Compiler::new("$a: 1px; b { c: $a; }", &options);
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();
    assert!(run.evaluator.variable_values().is_empty());
}

/// Recorded values include variables whose scope is gone by the end of the
/// compile, keyed by the declaration's own span, in source order.
#[test]
fn record_variable_values_captures_scoped_declarations() {
    let src = "\
$top: 1px;
.a {
  $inner: $top + 1px;
  width: $inner;
}
@each $i in 1, 2 {
  $loop: $i;
}
";
    let options = Options::default()
        .record_variable_values(true)
        .with_url("in.scss");
    let compiler = Compiler::new(src, &options);
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();

    let recorded: Vec<(&str, u32, u32, String)> = run
        .evaluator
        .variable_values()
        .values()
        .map(|v| (v.name.as_str(), v.span.line, v.span.col, v.value.to_css(false)))
        .collect();
    assert_eq!(
        recorded,
        vec![
            ("top", 0, 0, "1px".to_string()),
            // Rule-scoped: unreachable through `env` once the rule closes.
            ("inner", 2, 2, "2px".to_string()),
            // A loop body records its LAST visit.
            ("loop", 6, 2, "2".to_string()),
        ]
    );

    // The declaration span's file id resolves back to a source URL.
    let (span, _) = run.evaluator.variable_values().iter().next().unwrap();
    assert_eq!(run.evaluator.file_url(span.file), Some("in.scss"));

    // The scoped names really are gone from the environment.
    assert_eq!(
        run.evaluator.variable("top").map(|v| v.to_css(false)),
        Some("1px".to_string())
    );
    assert!(run.evaluator.variable("inner").is_none());
    assert!(run.evaluator.variable("loop").is_none());
    assert!(run.evaluator.env().contains_key("top"));
    assert!(!run.evaluator.env().contains_key("inner"));
}

/// A guarded (`!default`) declaration that is short-circuited still records the
/// value it resolves to, flagged as not self-evaluated.
#[test]
fn record_variable_values_marks_short_circuited_defaults() {
    let options = Options::default().record_variable_values(true);
    let compiler = Compiler::new("$a: 1px;\n$a: 2px !default;\n$b: 3px !default;\n", &options);
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();

    let recorded: Vec<(&str, bool, bool, String)> = run
        .evaluator
        .variable_values()
        .values()
        .map(|v| (v.name.as_str(), v.is_default, v.evaluated, v.value.to_css(false)))
        .collect();
    assert_eq!(
        recorded,
        vec![
            ("a", false, true, "1px".to_string()),
            // Guarded over an existing value: records the value in effect.
            ("a", true, false, "1px".to_string()),
            ("b", true, true, "3px".to_string()),
        ]
    );
}

/// Recording a declaration does not disturb the CSS it produces.
#[test]
fn record_variable_values_does_not_change_output() {
    let src = "$a: 1px !default;\n.x { $b: 2px; width: $a + $b; }\n";
    let plain = Options::default();
    let recording = Options::default().record_variable_values(true);
    assert_eq!(
        Compiler::new(src, &recording).compile().unwrap(),
        compile(src, &plain).unwrap()
    );
}

/// Values come back as live `Value`s, not strings, so tooling can inspect them
/// structurally.
#[test]
fn recorded_values_are_typed() {
    let options = Options::default().record_variable_values(true);
    let compiler = Compiler::new("$list: 1px 2px;\n$c: #336699;\n", &options);
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();

    let mut values = run.evaluator.variable_values().values();
    assert!(matches!(values.next().map(|v| &v.value), Some(Value::List(_))));
    assert!(matches!(values.next().map(|v| &v.value), Some(Value::Color(_))));
}

/// An in-memory importer, so the cross-file test needs no filesystem.
struct MemImporter(std::collections::HashMap<String, String>);

impl sasso::Importer for MemImporter {
    fn canonicalize(
        &self,
        url: &str,
        _ctx: &sasso::CanonicalizeContext<'_>,
    ) -> Result<Option<sasso::CanonicalUrl>, sasso::ImporterError> {
        Ok(self.0.contains_key(url).then(|| sasso::CanonicalUrl::new(url)))
    }

    fn load(
        &self,
        canonical: &sasso::CanonicalUrl,
    ) -> Result<Option<sasso::ImporterResult>, sasso::ImporterError> {
        Ok(self.0.get(canonical.as_str()).map(|c| sasso::ImporterResult {
            contents: c.clone(),
            syntax: sasso::Syntax::Scss,
            source_map_url: None,
        }))
    }
}

/// Declarations in a `@use`d file are recorded too, keyed by that file's own
/// interned id — which `file_url` resolves back to its URL.
#[test]
fn record_variable_values_spans_several_files() {
    let mut files = std::collections::HashMap::new();
    files.insert("lib".to_string(), "$from-lib: 7px;\n".to_string());
    let importer = MemImporter(files);

    let options = Options::default()
        .record_variable_values(true)
        .with_importer(&importer)
        .with_url("in.scss");
    let compiler = Compiler::new("@use \"lib\";\n$here: 1px;\n", &options);
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();

    let mut by_file: Vec<(Option<&str>, &str)> = run
        .evaluator
        .variable_values()
        .values()
        .map(|v| (run.evaluator.file_url(v.span.file), v.name.as_str()))
        .collect();
    by_file.sort();
    assert_eq!(
        by_file,
        vec![(Some("in.scss"), "here"), (Some("lib"), "from-lib")]
    );
}
