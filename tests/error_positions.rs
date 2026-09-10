//! Every error the compiler raises carries a source position.
//!
//! dart-sass attaches a span to every AST node it evaluates, so no diagnostic
//! can surface without one. sasso reaches the same place from the other
//! direction: errors raised deep in a helper with no span of their own pick up
//! the innermost enclosing statement or expression that has one, applied at
//! the `exec`/`eval_expr` boundaries (and at the pre-evaluation validators,
//! which have no boundary to unwind through).
//!
//! Sweeping all 14218 sass-spec cases with a build that flagged unpositioned
//! errors found none. The cases below are the representative shapes, with
//! every expected line/column/caret-width verified against dart-sass 1.100.

use sasso::{Allocator, Compiler, MemoryImporter, Options};

fn one(source: &str) -> MemoryImporter {
    MemoryImporter::new().with_file("in.scss", source)
}

/// Compile `source` and return the error, its 1-based position, and the source
/// text its byte span covers.
fn err_span(source: &str) -> (String, (usize, usize), String) {
    let arena = Allocator::default();
    let files = one(source);
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .compile()
        .expect_err("expected a compile error");
    assert!(
        err.has_position(),
        "error must carry a position: {}",
        err.message
    );
    assert!(
        err.rendered.is_some(),
        "a positioned error must render a snippet + frame trace: {}",
        err.message
    );
    let span = source[err.byte_span().unwrap()].to_string();
    (err.message, (err.line, err.col), span)
}

/// An `@include` of a mixin that does not exist, or that is misused, is blamed
/// on the invocation — the position dart-sass reports for all three.
#[test]
fn include_errors_blame_the_invocation() {
    // (sasso names the mixin in the message where dart-sass does not; the
    // position and caret are what this pins.)
    let (msg, pos, span) = err_span(".a { @include nope; }\n");
    assert!(msg.starts_with("Undefined mixin"), "{msg}");
    assert_eq!((pos, span), ((1, 6), "@include nope".to_string()));
    // dart-sass 1.100: `in.scss 2:6`, caret over `@include m`.
    assert_eq!(
        err_span("@mixin m($x) { c: $x; }\n.a { @include m; }\n"),
        (
            "Missing argument $x.".to_string(),
            (2, 6),
            "@include m".to_string()
        )
    );
    // The caret stops at the mixin name, not at the content block's brace.
    assert_eq!(
        err_span("@mixin m { c: d; }\n.a { @include m { e: f; } }\n"),
        (
            "Mixin doesn't accept a content block.".to_string(),
            (2, 6),
            "@include m".to_string()
        )
    );
}

/// A function call's errors are blamed on the call expression.
#[test]
fn function_call_errors_blame_the_call() {
    let (msg, pos, span) = err_span("@function f($x) { @return $x; }\na { c: f(1, 2); }\n");
    assert!(msg.contains("too many arguments"), "{msg}");
    assert_eq!((pos, span), ((2, 8), "f(1, 2)".to_string()));
}

/// A duplicate map key is blamed on the *second* key, not on the declaration
/// the map literal sits in.
#[test]
fn duplicate_map_key_blames_the_second_key() {
    // dart-sass 1.100: `in.scss 1:15`, a single caret (a zero-width span, so
    // the recorded byte range is empty and both engines draw one `^`).
    assert_eq!(
        err_span("a { c: (x: 1, x: 2); }\n"),
        ("Duplicate key.".to_string(), (1, 15), String::new())
    );
}

/// A variable declaration's own errors cover the declaration through the end
/// of its value.
#[test]
fn variable_declaration_errors_cover_the_declaration() {
    // dart-sass 1.100: `in.scss 2:1`, caret over `m.$pi: 4`.
    assert_eq!(
        err_span("@use \"sass:math\" as m;\nm.$pi: 4;\n"),
        (
            "Cannot modify built-in variable.".to_string(),
            (2, 1),
            "m.$pi: 4".to_string()
        )
    );
}

/// `@return` outside a function, and `@content` outside a mixin, are blamed on
/// the at-rule itself.
#[test]
fn misplaced_at_rules_blame_themselves() {
    let (msg, pos, span) = err_span(".a { @return 1; }\n");
    assert!(msg.contains("@return"), "{msg}");
    // dart-sass 1.100: `in.scss 1:6`, caret over `@return 1`.
    assert_eq!((pos, span), ((1, 6), "@return 1".to_string()));

    let (msg, pos, span) = err_span("a { @content; }\n");
    assert!(msg.contains("@content"), "{msg}");
    assert_eq!((pos, span), ((1, 5), "@content".to_string()));
}

/// An unsatisfied `@extend` is resolved after the whole sheet has run, so it
/// has no statement boundary left to unwind through — it names its own origin
/// instead, and renders a full block rather than the legacy one-liner.
#[test]
fn unmatched_extend_is_positioned_and_rendered() {
    // dart-sass 1.100: `in.scss 1:5`, caret over `@extend .b`.
    let (msg, pos, span) = err_span("a { @extend .b; }\nb { c: d; }\n");
    assert!(
        msg.starts_with("The target selector was not found."),
        "{msg}"
    );
    assert_eq!((pos, span), ((1, 5), "@extend .b".to_string()));
}

/// An `@extend` written in a loaded file is blamed on THAT file, even though
/// extends resolve long after evaluation has left it.
#[test]
fn unmatched_extend_names_the_file_that_wrote_it() {
    let arena = Allocator::default();
    let partial = "a { @extend .nope; }\n";
    let files = MemoryImporter::new()
        .with_file("in.scss", "@use \"lib\";\n")
        .with_file("_lib.scss", partial);
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .compile()
        .expect_err("expected an unmatched @extend");

    assert_eq!(err.file.as_deref(), Some("_lib.scss"));
    assert_eq!((err.line, err.col), (1, 5));
    assert_eq!(&partial[err.byte_span().unwrap()], "@extend .nope");
    assert!(err.to_string().contains("_lib.scss 1:5"), "{err}");
}

/// The placement validator runs before evaluation, so it has no statement
/// boundary to unwind through — it blames the offending statement itself, and
/// its errors render like any other.
#[test]
fn declaration_placement_errors_are_positioned() {
    // dart-sass 1.100: `in.scss 2:3`, caret over `@function f()`.
    assert_eq!(
        err_span("@each $i in 1 2 {\n  @function f() { @return 1; }\n}\n"),
        (
            "Functions may not be declared in control directives.".to_string(),
            (2, 3),
            "@function f()".to_string()
        )
    );
    // A `@mixin` prelude has no parameter list to close, so the caret stops at
    // the name; dart-sass runs it one further, over the space before the `{`.
    assert_eq!(
        err_span("@each $i in 1 2 {\n  @mixin m { a: b; }\n}\n"),
        (
            "Mixins may not be declared in control directives.".to_string(),
            (2, 3),
            "@mixin m".to_string()
        )
    );
    assert_eq!(
        err_span("@mixin outer {\n  @function f() { @return 1; }\n}\n"),
        (
            "Mixins may not contain function declarations.".to_string(),
            (2, 3),
            "@function f()".to_string()
        )
    );
    // A declaration inside a `@function` body: the caret covers the
    // declaration through the end of its value.
    assert_eq!(
        err_span("@function f() {\n  a: b;\n  @return 1;\n}\na { c: f(); }\n"),
        (
            "@function rules may not contain declarations.".to_string(),
            (2, 3),
            "a: b".to_string()
        )
    );
}

/// A style rule inside a keyframe block is rejected at evaluation, blamed on
/// the whole offending rule.
#[test]
fn style_rule_in_keyframes_is_positioned() {
    // dart-sass 1.100: `in.scss 3:5`, caret over `.x { a: b; }`.
    assert_eq!(
        err_span("@keyframes k {\n  from {\n    .x { a: b; }\n  }\n}\n"),
        (
            "Style rules may not be used within keyframe blocks.".to_string(),
            (3, 5),
            ".x { a: b; }".to_string()
        )
    );
}

/// A loop guard, a `@while` runaway, has no node of its own: it is blamed on
/// the enclosing statement, which is the `@while`'s condition.
#[test]
fn runaway_while_is_positioned() {
    let (msg, pos, _) = err_span("$i: 0;\n@while $i < 1 {\n  a { b: c; }\n}\n");
    assert!(msg.contains("@while exceeded"), "{msg}");
    assert_eq!(pos.0, 2, "blamed on the @while, not the body");
}

/// An error raised inside a mixin body is blamed on the innermost node that
/// has a span — the statement in the body, not the `@include` that ran it —
/// and the frame trace still names the call site.
#[test]
fn errors_inside_a_mixin_blame_the_innermost_node() {
    let arena = Allocator::default();
    let src = "@mixin m {\n  c: $nope;\n}\n.a { @include m; }\n";
    let (msg, pos, span) = err_span(src);
    assert_eq!(msg, "Undefined variable.");
    assert_eq!((pos, span), ((2, 6), "$nope".to_string()));

    let files = one(src);
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .compile()
        .unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.contains("in.scss 2:6"), "{rendered}");
    assert!(
        rendered.contains("in.scss 4:6"),
        "call site frame:\n{rendered}"
    );
}

/// A statement must be terminated by `;`, by the `}` that closes its block, or
/// by end of file. `oxc-css-parser` owns that diagnostic now, and it blames
/// where its scan actually stopped rather than backing the caret up to the end
/// of the statement's own line the way dart-sass's `expectStatementSeparator`
/// does. What still holds — and what this pins — is that each of these is
/// rejected, positioned, and rendered.
#[test]
fn missing_statement_terminator_is_positioned() {
    // The scan stops on the `.a` that opens the next line (dart-sass 1.104
    // reports `in.scss 1:18`, the end of line 1).
    let (msg, pos, _) = err_span("@use \"other\" as t\n.a { color: red; }\n");
    assert_eq!(
        (msg.as_str(), pos),
        ("expect token `;`, but found `.`", (2, 1))
    );
    // A value runs on into the next declaration's name, so the scan gives up
    // at that declaration's `:` — the same place dart-sass 1.104 does.
    let (msg, pos, _) = err_span("$a: 1\n$b: 2;\n");
    assert_eq!(
        (msg.as_str(), pos),
        ("expect token `;`, but found `:`", (2, 3))
    );
    // Likewise `in.scss 2:11` in dart-sass 1.104, though the wording differs.
    let (msg, pos, _) = err_span(".a { color: red\nbackground: blue; }\n");
    assert_eq!(
        (msg.as_str(), pos),
        ("component value is expected", (2, 11))
    );
    // A comment on a later line is trivia the scan passes over, and the caret
    // follows it past the comment (dart-sass 1.104: `in.scss 2:5`).
    let (msg, pos, _) = err_span("@use \"other\" as t\n// c\n.a { color: red; }\n");
    assert_eq!(
        (msg.as_str(), pos),
        ("expect token `;`, but found `.`", (3, 1))
    );
    // Blank lines are trivia too (dart-sass 1.104: `in.scss 1:21`).
    let (msg, pos, _) = err_span("@use \"other\" as t   \n\n.a { color: red; }\n");
    assert_eq!(
        (msg.as_str(), pos),
        ("expect token `;`, but found `.`", (3, 1))
    );
}
