//! A whole SCSS project compiled from a **virtual filesystem**, driven through
//! the staged [`Compiler`] API: `cargo run --example virtual_project`.
//!
//! The point of the staged API is that every source has a *name*. Nothing here
//! touches the disk — [`MemoryImporter`] holds a dozen small `.scss` files —
//! yet every diagnostic, every captured variable and every source-map entry is
//! attributed to the file it actually came from. The project deliberately
//! contains **two `_tokens.scss` partials in different directories** to show
//! that a shared basename does not merge them.
//!
//! It walks through, in order:
//!
//! 1. building the virtual project and compiling its entry file
//! 2. capturing every variable declaration, grouped by defining file
//! 3. rewriting the parsed AST before evaluation
//! 4. rewriting the evaluated output tree before emit, and emitting it twice
//! 5. a source map whose `sources` are the project's real file names
//! 6. an error inside one of the two same-named partials, blamed correctly

use std::collections::BTreeMap;

use sasso::ast::Stmt;
use sasso::{Compiler, MemoryImporter, Options, OutNode, OutputStyle, RuleSelectors};

fn main() {
    let files = project();

    section("1. compile the entry file");
    compile_entry(&files);

    section("2. capture every variable, by defining file");
    capture_variables(&files);

    section("3. rewrite the AST before evaluation");
    rewrite_ast(&files);

    section("4. rewrite the output tree, then emit it twice");
    rewrite_output(&files);

    section("5. source map: one entry per real file");
    source_map(&files);

    section("6. an error in one of two same-named partials");
    blame_the_right_file();
}

// ---------------------------------------------------------------------------
// The virtual project: a dozen small files across five directories.
// ---------------------------------------------------------------------------

/// A small but realistic 7-1-ish project. Every name here is what the compiler
/// will report in diagnostics, spans and source maps.
fn project() -> MemoryImporter {
    MemoryImporter::new()
        // The entry. Loads the shared abstracts, the base layer, two
        // components, a theme, and a vendored plain-CSS file.
        .with_file(
            "main.scss",
            r#"
@use "abstracts" as a;
@use "base/reset";
@use "components/button";
@use "components/card";
@use "themes/light" as theme;
@use "vendor/normalize.css";

$app-gutter: a.spacing(3);

.app {
  // Rule-scoped: this binding no longer exists once the rule closes, but it
  // is still captured, with its span, by `record_variable_values`.
  $inner-pad: a.spacing(2);

  background: theme.$bg;
  color: theme.$fg;
  padding: $inner-pad $app-gutter;
  border-radius: a.$radius;
}
"#,
        )
        // `abstracts/` is an index directory: `@use "abstracts"` resolves to
        // `abstracts/_index.scss`, which re-exports the three real partials.
        .with_file(
            "abstracts/_index.scss",
            r#"
@forward "variables";
@forward "functions";
@forward "mixins";
"#,
        )
        .with_file(
            "abstracts/_variables.scss",
            r#"
$base-unit: 4px !default;
$radius: 6px !default;
$brand: #336699 !default;
$breakpoints: (small: 480px, medium: 768px, large: 1200px);
"#,
        )
        .with_file(
            "abstracts/_functions.scss",
            r#"
@use "sass:map";
@use "variables" as v;

@function spacing($steps) {
  @return v.$base-unit * $steps;
}

@function breakpoint($name) {
  @return map.get(v.$breakpoints, $name);
}
"#,
        )
        .with_file(
            "abstracts/_mixins.scss",
            r#"
@use "functions" as fn;

@mixin respond-to($name) {
  @media (min-width: #{fn.breakpoint($name)}) {
    @content;
  }
}

@mixin surface($bg, $fg) {
  background: $bg;
  color: $fg;
}
"#,
        )
        .with_file(
            "base/_reset.scss",
            r#"
%box { box-sizing: border-box; margin: 0; }

html, body { @extend %box; }
"#,
        )
        // Two `_tokens.scss`, in two directories. Same basename, different
        // files — and the compiler keeps them apart everywhere.
        .with_file(
            "themes/_tokens.scss",
            r#"
$bg: #ffffff;
$fg: #1a1a1a;
$muted: #6b7280;
"#,
        )
        .with_file(
            "components/_tokens.scss",
            r#"
$gap: 8px;
$border: 1px;
"#,
        )
        // A relative `@use "tokens"` inside `themes/` finds `themes/_tokens.scss`,
        // not `components/_tokens.scss`: relative URLs resolve against the
        // containing file.
        .with_file(
            "themes/_light.scss",
            r#"
@use "tokens" as t;
@use "../abstracts" as a;

$bg: t.$bg;
$fg: t.$fg;
$accent: a.$brand;
"#,
        )
        .with_file(
            "components/_button.scss",
            r#"
@use "../abstracts" as a;
@use "tokens" as t;
@use "../themes/light" as theme;

.btn {
  @include a.surface(theme.$accent, #fff);
  padding: a.spacing(1) a.spacing(2);
  border: t.$border solid theme.$accent;
  border-radius: a.$radius;

  @include a.respond-to(medium) {
    padding: a.spacing(2) a.spacing(3);
  }
}
"#,
        )
        .with_file(
            "components/_card.scss",
            r#"
@use "../abstracts" as a;
@use "tokens" as t;
@use "../themes/light" as theme;

.card {
  display: grid;
  gap: t.$gap;
  background: theme.$bg;
  color: theme.$fg;
  border-radius: a.$radius;
}

@each $name, $steps in (sm: 1, md: 2, lg: 3) {
  // Loop-scoped: captured once per visit, so the record holds its LAST value.
  $pad: a.spacing($steps);

  .card--#{$name} { padding: $pad; }
}
"#,
        )
        // A plain-CSS file: the importer reports `Syntax::Css` from the `.css`
        // extension, so it is passed through without SassScript evaluation.
        .with_file("vendor/normalize.css", "button { font-family: inherit; }\n")
}

// ---------------------------------------------------------------------------
// 1. Compile.
// ---------------------------------------------------------------------------

fn compile_entry(files: &MemoryImporter) {
    let options = Options::default().with_importer(files);
    let compiler = Compiler::new("main.scss", &options).expect("entry resolves");

    // The entry went through the importer like any other load, so it has a
    // canonical URL and a syntax of its own.
    println!("entry canonical url : {}", compiler.entry().as_str());
    println!("entry syntax        : {:?}", compiler.syntax());
    println!("files in project    : {}\n", files.names().count());

    println!("{}", compiler.compile().expect("compiles"));
}

// ---------------------------------------------------------------------------
// 2. Variable capture.
// ---------------------------------------------------------------------------

/// `Options::record_variable_values` records every variable declaration as it
/// is evaluated — including ones whose scope is gone by the end of the compile
/// (a `@each` body, a rule-local `$x`), which no environment lookup could find
/// afterwards. Each record carries the span it was declared at — with a byte
/// offset alongside the line/column — the resolved selector of the rule it sits
/// in, and a file id that resolves back to that file's canonical URL.
fn capture_variables(files: &MemoryImporter) {
    let options = Options::default()
        .with_importer(files)
        .record_variable_values(true);
    let compiler = Compiler::new("main.scss", &options).expect("entry resolves");
    let sheet = compiler.parse().expect("parses");
    let run = compiler.evaluate(&sheet).expect("evaluates");

    // Group by defining file. `variable_values()` iterates in source order
    // (file id, line, column), so each file's list is already ordered.
    let mut by_file: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for v in run.evaluator.variable_values().values() {
        let file = run.evaluator.file_url(v.span.file).unwrap_or("<unknown>");
        // Spans are 0-based; diagnostics are 1-based.
        let guard = if v.is_default { " !default" } else { "" };
        // The declaration's scope: the enclosing style rule's resolved
        // selector, or the document root.
        let scope = v
            .selector
            .as_deref()
            .map_or_else(|| "(root)".to_string(), |sel| sel.join(", "));
        by_file.entry(file).or_default().push(format!(
            "{:>3}:{:<3} ${} = {}{}   in {scope}",
            v.span.line + 1,
            v.span.col + 1,
            v.name,
            v.value.to_css(false),
            guard,
        ));
    }
    for (file, vars) in &by_file {
        println!("{file}");
        for v in vars {
            println!("    {v}");
        }
    }

    // The two same-named partials are separate entries above, and neither one
    // shadowed the other.
    let themes = by_file.get("themes/_tokens.scss").expect("themes tokens");
    let components = by_file.get("components/_tokens.scss").expect("components tokens");
    assert!(themes.iter().any(|v| v.contains("$bg")));
    assert!(components.iter().any(|v| v.contains("$gap")));
    println!(
        "\nboth _tokens.scss files captured separately: {} + {} declarations",
        themes.len(),
        components.len()
    );

    // A top-level entry variable is still in the environment afterwards; the
    // rule-scoped one is not — yet both were captured above.
    println!(
        "\n$app-gutter still in env : {:?}",
        run.evaluator.variable("app-gutter").map(|v| v.to_css(false))
    );
    println!(
        "$inner-pad still in env  : {:?}",
        run.evaluator.variable("inner-pad").map(|v| v.to_css(false))
    );
}

// ---------------------------------------------------------------------------
// 3. AST rewriting.
// ---------------------------------------------------------------------------

/// `parse` hands back the statement tree of the entry file, mutable. Here the
/// vendored plain-CSS `@use` is dropped before evaluation, so its rule never
/// reaches the output.
fn rewrite_ast(files: &MemoryImporter) {
    let options = Options::default().with_importer(files);
    let compiler = Compiler::new("main.scss", &options).expect("entry resolves");
    let mut sheet = compiler.parse().expect("parses");

    let before = sheet.stmts.len();
    sheet
        .stmts
        .retain(|s| !matches!(s, Stmt::Use { url, .. } if url.ends_with(".css")));
    println!(
        "top-level statements: {before} -> {} (dropped the vendored @use)",
        sheet.stmts.len()
    );

    let run = compiler.evaluate(&sheet).expect("evaluates");
    let css = compiler.emit(&run.nodes);
    assert!(!css.contains("font-family: inherit"));
    println!(
        "vendor rule gone from the output: {}",
        !css.contains("font-family")
    );
}

// ---------------------------------------------------------------------------
// 4. Output-tree rewriting, and re-emitting one tree.
// ---------------------------------------------------------------------------

/// `evaluate` hands back the flattened output tree — resolved selectors and
/// all — which can be rewritten before serialization. One tree can then be
/// emitted as many times as you like, under different options.
fn rewrite_output(files: &MemoryImporter) {
    let expanded = Options::default().with_importer(files);
    let compiler = Compiler::new("main.scss", &expanded).expect("entry resolves");
    let sheet = compiler.parse().expect("parses");
    let mut run = compiler.evaluate(&sheet).expect("evaluates");

    // Namespace the whole stylesheet under `.v2`, the way a CSS-in-JS scoper
    // would — but on resolved selectors, after `@extend` has run. Each `@use`d
    // module's CSS sits in its own `ModuleScope`, and bubbled `@media` blocks
    // hold their rules in an `AtRule` body, so this walks the tree.
    let scoped = scope_rules(&mut run.nodes, ".v2");
    println!("scoped {scoped} rules under `.v2`");

    // The same tree, serialized twice.
    let pretty = compiler.emit(&run.nodes);
    let compressed = Compiler::new(
        "main.scss",
        &Options::default()
            .with_importer(files)
            .with_style(OutputStyle::Compressed),
    )
    .expect("entry resolves")
    .emit(&run.nodes);

    println!("expanded   : {} bytes", pretty.len());
    println!("compressed : {} bytes", compressed.len());
    println!("first rule : {}", pretty.lines().next().unwrap_or_default());
}

/// Prefix every style rule in `nodes` — at any depth — with `prefix`, and
/// report how many were rewritten.
fn scope_rules(nodes: &mut [OutNode], prefix: &str) -> usize {
    let mut count = 0;
    for node in nodes {
        match node {
            OutNode::Rule { selectors, .. } => {
                *selectors = RuleSelectors::Raw(
                    selectors
                        .to_strings()
                        .iter()
                        .map(|s| format!("{prefix} {s}"))
                        .collect(),
                );
                count += 1;
            }
            // A module's spliced CSS, and a bubbled `@media` / `@supports` body.
            OutNode::ModuleScope { nodes, .. } | OutNode::AtRule { body: nodes, .. } => {
                count += scope_rules(nodes, prefix);
            }
            _ => {}
        }
    }
    count
}

// ---------------------------------------------------------------------------
// 5. Source maps.
// ---------------------------------------------------------------------------

/// Because file identity is the importer's canonical URL, the map's `sources`
/// are the project's real names — `themes/_tokens.scss` and
/// `components/_tokens.scss` are two entries, each with its own text.
fn source_map(files: &MemoryImporter) {
    let options = Options::default()
        .with_importer(files)
        .with_source_map_include_sources(true);
    let compiler = Compiler::new("main.scss", &options)
        .expect("entry resolves")
        .with_source_map(true);
    let sheet = compiler.parse().expect("parses");
    let mut run = compiler.evaluate(&sheet).expect("evaluates");
    let result = compiler.emit_with_source_map(&run.nodes, &mut run.evaluator);

    let contents = result.source_map.sources_content.as_deref().unwrap_or_default();
    for (i, src) in result.source_map.sources.iter().enumerate() {
        let bytes = contents.get(i).map(String::len).unwrap_or(0);
        println!("  sources[{i}] = {src} ({bytes} bytes of embedded text)");
    }
    println!("  mappings: {} bytes", result.source_map.mappings.len());
}

// ---------------------------------------------------------------------------
// 6. Error attribution.
// ---------------------------------------------------------------------------

/// The same project, but `components/_tokens.scss` refers to a variable that
/// does not exist. The diagnostic must name *that* file — not the
/// `themes/_tokens.scss` it shares a basename with — and the snippet must be
/// its own text.
fn blame_the_right_file() {
    let mut broken = project();
    broken.insert(
        "components/_tokens.scss",
        "$gap: 8px;\n$border: $undefined-token;\n",
    );

    let options = Options::default().with_importer(&broken);
    let err = Compiler::new("main.scss", &options)
        .expect("entry resolves")
        .compile()
        .expect_err("the project no longer compiles");

    println!("{err}");
    assert!(err.to_string().contains("components/_tokens.scss"));
}

// ---------------------------------------------------------------------------

fn section(title: &str) {
    println!("\n{}\n=== {title} ===", "-".repeat(70));
}
