//! The staged compile API: parse -> (inspect/modify) -> evaluate ->
//! (inspect/modify) -> emit, plus `Options::record_variable_values`.
//!
//! These gate the *embedder-facing* contract that the intermediate forms are
//! reachable and mutable, that routing the one-shot `compile` through the same
//! three steps still produces byte-identical CSS, and that every source the
//! staged compiler touches is a NAMED file whose diagnostics, spans and
//! source-map entries are attributed to it and not to a same-named sibling.

use sasso::ast::{Expr, Stmt, TplPiece};
use sasso::selector::{Combinator, Complex, ComplexComponent, Compound, Simple};
use sasso::value::Value;
use sasso::{compile, Compiler, MemoryImporter, Options, OutNode, OutputStyle, RuleSelectors};

/// A one-file virtual filesystem, the shape most of these tests need.
fn one(source: &str) -> MemoryImporter {
    MemoryImporter::new().with_file("in.scss", source)
}

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
    let files = one(src);
    for style in [OutputStyle::Expanded, OutputStyle::Compressed] {
        let options = Options::default().with_style(style).with_importer(&files);
        let compiler = Compiler::new("in.scss", &options).unwrap();
        let sheet = compiler.parse().unwrap();
        let run = compiler.evaluate(&sheet).unwrap();
        assert_eq!(compiler.emit(&run.nodes), compile(src, &options).unwrap());
    }
}

/// The entry file is loaded through the importer, and its canonical URL — not
/// the spelling passed in — becomes the compile's identity.
#[test]
fn entry_is_resolved_through_the_importer() {
    let files = MemoryImporter::new().with_file("ui/_button.scss", ".btn { color: red; }");
    let options = Options::default().with_importer(&files);

    // Partial resolution applies to the entry exactly as it does to a `@use`.
    let compiler = Compiler::new("ui/button", &options).unwrap();
    assert_eq!(compiler.entry().as_str(), "ui/_button.scss");
    assert_eq!(compiler.url(), "ui/button");
    assert_eq!(compiler.source(), ".btn { color: red; }");
    assert_eq!(compiler.compile().unwrap(), ".btn {\n  color: red;\n}");
}

/// Without an importer there is no filesystem to name an entry in, so
/// construction fails rather than silently compiling nothing.
#[test]
fn entry_requires_an_importer() {
    let options = Options::default();
    let err = Compiler::new("in.scss", &options).expect_err("expected a missing-importer error");
    assert!(err.message.contains("No importer configured"), "{err}");
}

/// A missing entry is an error, and it names the file that was asked for.
#[test]
fn missing_entry_is_reported() {
    let files = one("a { color: red; }");
    let options = Options::default().with_importer(&files);
    let err = Compiler::new("nope.scss", &options).expect_err("expected a not-found error");
    assert!(err.message.contains("nope.scss"), "{err}");
}

/// The importer decides each file's syntax; a `.css` entry is plain CSS.
#[test]
fn entry_syntax_comes_from_the_importer() {
    let files = MemoryImporter::new().with_file("in.css", "a { color: red; }");
    let options = Options::default().with_importer(&files);
    let compiler = Compiler::new("in.css", &options).unwrap();
    assert_eq!(compiler.syntax(), sasso::Syntax::Css);
    assert_eq!(compiler.compile().unwrap(), "a {\n  color: red;\n}");
}

/// A parse error surfaces from `parse`, not from `evaluate`, and renders
/// against the entry file's name without any `Options::url` being set.
#[test]
fn staged_parse_reports_syntax_errors() {
    let files = one("a { color: ");
    let options = Options::default().with_importer(&files);
    let err = Compiler::new("in.scss", &options)
        .unwrap()
        .parse()
        .expect_err("expected a parse error");
    assert!(err.line > 0, "expected a positioned parse error: {err}");
    assert!(
        err.to_string().contains("in.scss 1:12"),
        "parse error must name the entry file: {err}"
    );
}

/// The declaration-placement check runs on the sheet handed to `evaluate`, so a
/// caller-built violation is still rejected.
#[test]
fn staged_evaluate_validates_modified_sheet() {
    let files = one("@mixin m { a { b: c; } }");
    let options = Options::default().with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
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
    let files = one("a { color: red; }\nb { color: blue; }");
    let options = Options::default().with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
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
    let files = one("a { color: red; }");
    let options = Options::default().with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
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
    let files = one("a { color: red; }");
    let options = Options::default().with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
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
    let files = one(src);
    let expanded = Options::default().with_importer(&files);
    let compiler = Compiler::new("in.scss", &expanded).unwrap();
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();

    let compressed = Options::default()
        .with_style(OutputStyle::Compressed)
        .with_importer(&files);
    assert_eq!(compiler.emit(&run.nodes), "a {\n  color: red;\n}");
    assert_eq!(
        Compiler::new("in.scss", &compressed).unwrap().emit(&run.nodes),
        compile(src, &compressed).unwrap()
    );
}

/// Nothing is recorded unless the option is set.
#[test]
fn variable_values_are_not_recorded_by_default() {
    let files = one("$a: 1px; b { c: $a; }");
    let options = Options::default().with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
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
    let files = one(src);
    let options = Options::default()
        .record_variable_values(true)
        .with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();

    let recorded: Vec<(&str, u32, u32, String)> = run
        .evaluator
        .variable_values()
        .values()
        .map(|v| (v.name.as_str(), v.span.line, v.span.col, v.value.to_css(false)))
        .collect();
    // Spans carry 1-based line/column, exactly like a diagnostic's position.
    assert_eq!(
        recorded,
        vec![
            ("top", 1, 1, "1px".to_string()),
            // Rule-scoped: unreachable through `env` once the rule closes.
            ("inner", 3, 3, "2px".to_string()),
            // A loop body records its LAST visit.
            ("loop", 7, 3, "2".to_string()),
        ]
    );

    // The declaration span's file id resolves back to the entry's canonical URL.
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
    let files = one("$a: 1px;\n$a: 2px !default;\n$b: 3px !default;\n");
    let options = Options::default()
        .record_variable_values(true)
        .with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
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
    let files = one(src);
    let plain = Options::default();
    let recording = Options::default()
        .record_variable_values(true)
        .with_importer(&files);
    assert_eq!(
        Compiler::new("in.scss", &recording).unwrap().compile().unwrap(),
        compile(src, &plain).unwrap()
    );
}

/// Values come back as live `Value`s, not strings, so tooling can inspect them
/// structurally.
#[test]
fn recorded_values_are_typed() {
    let files = one("$list: 1px 2px;\n$c: #336699;\n");
    let options = Options::default()
        .record_variable_values(true)
        .with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();

    let mut values = run.evaluator.variable_values().values();
    assert!(matches!(values.next().map(|v| &v.value), Some(Value::List(_))));
    assert!(matches!(values.next().map(|v| &v.value), Some(Value::Color(_))));
}

/// Declarations in a `@use`d file are recorded too, keyed by that file's own
/// interned id — which `file_url` resolves back to its canonical URL.
#[test]
fn record_variable_values_spans_several_files() {
    let files = MemoryImporter::new()
        .with_file("in.scss", "@use \"lib\";\n$here: 1px;\n")
        .with_file("_lib.scss", "$from-lib: 7px;\n");
    let options = Options::default()
        .record_variable_values(true)
        .with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
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
        vec![(Some("_lib.scss"), "from-lib"), (Some("in.scss"), "here")]
    );
}

/// Two partials with the SAME basename in different directories are distinct
/// files: distinct interned ids, so neither their captured variables nor their
/// source-map entries collapse into one.
#[test]
fn same_named_partials_in_different_directories_stay_distinct() {
    let files = MemoryImporter::new()
        .with_file(
            "in.scss",
            "@use \"theme/colors\" as t;\n@use \"base/colors\" as b;\na { x: t.$c; y: b.$c; }\n",
        )
        .with_file("theme/_colors.scss", "$c: red;\n$only-theme: 1px;\n")
        .with_file("base/_colors.scss", "$c: blue;\n$only-base: 2px;\n");
    let options = Options::default()
        .record_variable_values(true)
        .with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();

    assert_eq!(compiler.emit(&run.nodes), "a {\n  x: red;\n  y: blue;\n}");

    // All four declarations survive: a per-file span means `$c` from `theme`
    // and `$c` from `base` are different keys.
    let mut captured: Vec<(&str, &str)> = run
        .evaluator
        .variable_values()
        .values()
        .map(|v| (run.evaluator.file_url(v.span.file).unwrap_or(""), v.name.as_str()))
        .collect();
    captured.sort();
    assert_eq!(
        captured,
        vec![
            ("base/_colors.scss", "c"),
            ("base/_colors.scss", "only-base"),
            ("theme/_colors.scss", "c"),
            ("theme/_colors.scss", "only-theme"),
        ]
    );
}

/// An error inside one of two same-named partials is attributed to the right
/// one: the frame prints its full relative URL and the snippet is ITS text.
#[test]
fn errors_name_the_file_they_came_from() {
    let files = MemoryImporter::new()
        .with_file(
            "in.scss",
            "@use \"theme/colors\" as t;\n@use \"base/colors\" as b;\n",
        )
        .with_file("theme/_colors.scss", "$c: red;\n")
        .with_file("base/_colors.scss", "$b: 2px;\n$x: $undefined-here;\n");
    let options = Options::default().with_importer(&files);
    let err = Compiler::new("in.scss", &options)
        .unwrap()
        .compile()
        .expect_err("expected an evaluation error");
    let rendered = err.to_string();
    assert!(
        rendered.contains("base/_colors.scss 2:5"),
        "frame must name the erroring partial, not its namesake:\n{rendered}"
    );
    assert!(
        rendered.contains("$x: $undefined-here;"),
        "snippet must come from the erroring partial's own text:\n{rendered}"
    );
    assert!(
        rendered.contains("in.scss 2:1"),
        "the loader frame must name the entry:\n{rendered}"
    );
}

/// The source map lists every file by its canonical URL, with each file's own
/// text under `sourcesContent` — same-named partials included.
#[test]
fn source_map_sources_are_per_file() {
    let files = MemoryImporter::new()
        .with_file(
            "in.scss",
            "@use \"theme/colors\" as t;\n@use \"base/colors\" as b;\n",
        )
        .with_file("theme/_colors.scss", ".t { color: red; }\n")
        .with_file("base/_colors.scss", ".b { color: blue; }\n");
    let options = Options::default()
        .with_importer(&files)
        .with_source_map_include_sources(true);
    let compiler = Compiler::new("in.scss", &options).unwrap().with_source_map(true);
    let sheet = compiler.parse().unwrap();
    let mut run = compiler.evaluate(&sheet).unwrap();
    let result = compiler.emit_with_source_map(&run.nodes, &mut run.evaluator);

    let sources = &result.source_map.sources;
    assert!(
        sources.contains(&"theme/_colors.scss".to_string())
            && sources.contains(&"base/_colors.scss".to_string()),
        "sources must be the canonical URLs: {sources:?}"
    );
    let contents = result.source_map.sources_content.expect("sourcesContent");
    let theme = sources.iter().position(|s| s == "theme/_colors.scss").unwrap();
    let base = sources.iter().position(|s| s == "base/_colors.scss").unwrap();
    assert_eq!(contents[theme], ".t { color: red; }\n");
    assert_eq!(contents[base], ".b { color: blue; }\n");
}

/// An `@import` (textual, not a module) also swaps file identity, so a span
/// stamped inside the imported file interns under ITS canonical URL.
#[test]
fn imported_files_keep_their_own_identity() {
    let files = MemoryImporter::new()
        .with_file("in.scss", "@import \"a/vars\";\n@import \"b/vars\";\n")
        .with_file("a/_vars.scss", "$from-a: 1px;\n")
        .with_file("b/_vars.scss", "$from-b: 2px;\n");
    let options = Options::default()
        .record_variable_values(true)
        .with_importer(&files);
    let compiler = Compiler::new("in.scss", &options).unwrap();
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(&sheet).unwrap();

    let mut captured: Vec<(&str, &str)> = run
        .evaluator
        .variable_values()
        .values()
        .map(|v| (run.evaluator.file_url(v.span.file).unwrap_or(""), v.name.as_str()))
        .collect();
    captured.sort();
    assert_eq!(
        captured,
        vec![("a/_vars.scss", "from-a"), ("b/_vars.scss", "from-b")]
    );
}

/// Relative loads resolve against the *containing* file, so a partial's own
/// `@use "./x"` finds its sibling rather than the entry's.
#[test]
fn relative_loads_resolve_against_the_containing_file() {
    let files = MemoryImporter::new()
        .with_file("in.scss", "@use \"pkg/entry\" as p;\na { x: p.$v; }\n")
        .with_file("pkg/_entry.scss", "@use \"local\" as l;\n$v: l.$v;\n")
        .with_file("pkg/_local.scss", "$v: from-pkg;\n")
        .with_file("_local.scss", "$v: from-root;\n");
    let options = Options::default().with_importer(&files);
    let css = Compiler::new("in.scss", &options).unwrap().compile().unwrap();
    assert_eq!(css, "a {\n  x: from-pkg;\n}");
}
