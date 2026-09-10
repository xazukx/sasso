//! Source-span plumbing: byte offsets, error file attribution, and the
//! positions the tree used to leave unrecorded.
//!
//! Every expected line/column here was produced by dart-sass 1.100 and
//! verified against it; the byte offsets are sasso's own addition (dart's
//! `SourceSpan` carries them too, they were simply not surfaced before).

use sasso::ast::{AtRulePrelude, ImportPreludeHref, InterpolableStr, Statement};
use sasso::{Allocator, Compiler, MemoryImporter, Options};

/// A one-file virtual filesystem, the shape most of these tests need.
fn one(source: &str) -> MemoryImporter {
    MemoryImporter::new().with_file("in.scss", source)
}

/// An error carries its span in both flavours — 1-based line/column for
/// display, byte offsets for slicing — plus the file the position is in.
#[test]
fn error_carries_a_byte_span_and_a_file() {
    let arena = Allocator::default();
    let src = "a {\n  color: $missing;\n}\n";
    let files = one(src);
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .compile()
        .expect_err("expected an undefined-variable error");

    assert_eq!((err.line, err.col), (2, 10));
    assert_eq!(err.length, "$missing".len());
    assert_eq!(err.end - err.start, err.length);
    assert_eq!(&src[err.start..err.end], "$missing");
    assert_eq!(&src[err.byte_span().unwrap()], "$missing");
    assert_eq!(err.file.as_deref(), Some("in.scss"));
}

/// A non-ASCII prefix pushes the byte offset past the character column, which
/// is the whole point of carrying both.
#[test]
fn byte_offsets_are_bytes_not_characters() {
    let arena = Allocator::default();
    let src = "a {\n  /* h\u{e9}llo w\u{f6}rld */ color: $missing;\n}\n";
    let files = one(src);
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .compile()
        .expect_err("expected an undefined-variable error");

    assert_eq!(&src[err.byte_span().unwrap()], "$missing");
    // Two 2-byte characters precede it on the line, so the byte offset has run
    // two ahead of the (character-counted) column.
    assert_eq!(err.start, 4 + err.col + 2 - 1);
}

/// A parse error is attributed to the entry file and spans bytes the same way.
#[test]
fn parse_errors_carry_a_byte_span_and_a_file() {
    let arena = Allocator::default();
    let src = "a !important {\n  color: red;\n}\n";
    let files = one(src);
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .parse()
        .expect_err("expected a parse error");

    // `oxc-css-parser` owns the wording of a syntax error, so it is its own
    // rather than dart-sass's `expected "{".`. What this test pins is the
    // plumbing around it: positioned, attributed, and byte-sliceable.
    assert_eq!(err.message, "expect token `{`, but found `!`");
    assert!(err.has_position());
    assert_eq!(err.file.as_deref(), Some("in.scss"));
    assert_eq!(src.as_bytes()[err.byte_span().unwrap().start], b'!');
}

/// An error raised inside a loaded file is attributed to THAT file, not the
/// entry — and its byte span indexes that file's source.
#[test]
fn error_file_is_the_file_the_position_is_in() {
    let arena = Allocator::default();
    let partial = "$b: 2px;\n$x: $undefined-here;\n";
    let files = MemoryImporter::new()
        .with_file("in.scss", "@use \"base/colors\" as b;\n")
        .with_file("base/_colors.scss", partial);
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .compile()
        .expect_err("expected an evaluation error");

    assert_eq!(err.file.as_deref(), Some("base/_colors.scss"));
    assert_eq!(&partial[err.byte_span().unwrap()], "$undefined-here");
}

/// A bare declaration directly inside a top-level `@media` is invalid, and the
/// error now says where: dart-sass blames the point its selector scan gives up
/// on the declaration text, i.e. the `;`.
#[test]
fn declaration_in_a_top_level_media_is_positioned() {
    let arena = Allocator::default();
    let src = "@media screen {\n  color: red;\n}\n";
    let files = one(src);
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .compile()
        .expect_err("a declaration may not sit directly in a top-level @media");

    assert_eq!(err.message, "expected \"{\".");
    // dart-sass 1.100: `in.scss 2:13`.
    assert_eq!((err.line, err.col), (2, 13));
    assert_eq!(err.file.as_deref(), Some("in.scss"));
    assert_eq!(src.as_bytes()[err.start], b';');
    assert!(
        err.to_string().contains("in.scss 2:13  root stylesheet"),
        "{err}"
    );
}

/// An `!important` flag moves the blame to the `!`: dart's scan stops there
/// rather than at the terminator.
#[test]
fn top_level_media_declaration_with_a_flag_blames_the_bang() {
    let arena = Allocator::default();
    let files = one("@media screen {\n  color: red !important;\n}\n");
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .compile()
        .unwrap_err();
    // dart-sass 1.100: `in.scss 2:14`.
    assert_eq!((err.line, err.col), (2, 14));
}

/// The same error from a top-level declaration in an `@import`ed file names
/// that file and keeps the `@import` frame.
#[test]
fn top_level_declaration_in_an_imported_file_is_positioned() {
    let arena = Allocator::default();
    let files = MemoryImporter::new()
        .with_file("in.scss", "@import \"bad\";\n")
        .with_file("_bad.scss", "a: b;\n");
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .compile()
        .expect_err("a top-level declaration in an imported file is invalid");

    // The parser rejects this one before the evaluator ever sees it, so both
    // the wording and the caret are `oxc-css-parser`'s (dart-sass 1.100 says
    // `expected "{".` at `_bad.scss 1:5`, having re-read the text as a
    // selector). The frames are what this test is for.
    assert_eq!(err.message, "declaration at top level is disallowed");
    assert_eq!((err.line, err.col), (1, 1));
    assert_eq!(err.file.as_deref(), Some("_bad.scss"));
    let rendered = err.to_string();
    assert!(rendered.contains("_bad.scss 1:1  @import"), "{rendered}");
    assert!(rendered.contains("in.scss 1:9"), "{rendered}");
}

/// A `ns.$var` reference is blamed on the whole reference, starting at the
/// namespace — dart-sass underlines `b.$nope`, not just the `$nope`.
#[test]
fn namespaced_variable_reference_is_positioned() {
    let arena = Allocator::default();
    let src = "@use \"lib\" as b;\nx {\n  c: b.$nope;\n}\n";
    let files = MemoryImporter::new()
        .with_file("in.scss", src)
        .with_file("_lib.scss", "$a: 1;\n");
    let options = Options::default().with_importer(&files);
    let err = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .compile()
        .expect_err("expected an undefined-variable error");

    assert_eq!(err.message, "Undefined variable.");
    // dart-sass 1.100: `in.scss 3:6`, caret over all of `b.$nope`.
    assert_eq!((err.line, err.col), (3, 6));
    assert_eq!(&src[err.byte_span().unwrap()], "b.$nope");
}

/// A plain-CSS `@import` carries the span of its URL token, exactly like a
/// Sass one. Nothing in compilation resolves it, so this exists purely so a
/// tool can locate every import in the tree.
#[test]
fn plain_css_import_records_its_position() {
    let arena = Allocator::default();
    let src = "@import \"a\";\n@import url(b.css);\n@import \"c.css\" screen;\n";
    let files = MemoryImporter::new()
        .with_file("in.scss", src)
        .with_file("_a.scss", "");
    let options = Options::default().with_importer(&files);
    let parsed = Compiler::new(&arena, "in.scss", &options)
        .unwrap()
        .parse()
        .unwrap();

    let mut spans = Vec::new();
    for stmt in &parsed.stylesheet.statements {
        let Statement::AtRule(at) = stmt else {
            continue;
        };
        match &at.prelude {
            // The Sass-only shape: a comma-separated list of quoted URLs, each
            // with a span of its own.
            Some(AtRulePrelude::SassImport(import)) => {
                spans.extend(import.paths.iter().map(|p| p.span));
            }
            // Anything that shape doesn't cover — a `url()`, a media-query
            // tail — is a plain-CSS `@import`, and the href holds the span.
            Some(AtRulePrelude::Import(import)) => spans.push(match &import.href {
                ImportPreludeHref::Str(InterpolableStr::Literal(s)) => s.span,
                ImportPreludeHref::Url(url) => url.span,
                other => panic!("unexpected @import href: {other:?}"),
            }),
            _ => {}
        }
    }

    let located: Vec<_> = spans
        .iter()
        .map(|s| {
            let (line, col) = line_col(src, s.start);
            (line, col, &src[s.start..s.end])
        })
        .collect();
    assert_eq!(
        located,
        vec![(1, 9, "\"a\""), (2, 9, "url(b.css)"), (3, 9, "\"c.css\"")]
    );
}

/// The 1-based line and (character-counted) column of a byte offset — what a
/// tool would render, and what the error type reports for a diagnostic.
fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let before = &src[..offset];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (
        before.matches('\n').count() + 1,
        before[line_start..].chars().count() + 1,
    )
}

/// A recorded variable declaration knows the style rule it was declared in —
/// including one whose rule emits no CSS at all, which is exactly the case an
/// output-tree scan cannot recover.
#[test]
fn recorded_variables_know_their_enclosing_selector() {
    let arena = Allocator::default();
    let src = "\
$top: 1px;
.a {
  $inner: 2px;
  width: $inner;
  &:hover .b, .c {
    $deep: 3px;
    width: $deep;
  }
}
.empty {
  $invisible: 4px;
}
@mixin m { $from-mixin: 5px; }
.caller { @include m; width: 6px; }
@media (min-width: 1px) {
  .m { $in-media: 7px; width: $in-media; }
}
";
    let files = one(src);
    let options = Options::default()
        .record_variable_values(true)
        .with_importer(&files);
    let compiler = Compiler::new(&arena, "in.scss", &options).unwrap();
    let sheet = compiler.parse().unwrap();
    let run = compiler.evaluate(sheet).unwrap();

    let recorded: Vec<(&str, Option<Vec<String>>)> = run
        .evaluator
        .variable_values()
        .values()
        .map(|v| (v.name.as_str(), v.selector.clone()))
        .collect();
    assert_eq!(
        recorded,
        vec![
            ("top", None),
            ("inner", Some(vec![".a".to_string()])),
            // `&` and the comma list are already resolved.
            (
                "deep",
                Some(vec![".a:hover .b".to_string(), ".a .c".to_string()])
            ),
            // The rule emits nothing, so there is no output node to recover
            // this from.
            ("invisible", Some(vec![".empty".to_string()])),
            // A mixin body reports the selector of the rule that included it.
            ("from-mixin", Some(vec![".caller".to_string()])),
            ("in-media", Some(vec![".m".to_string()])),
        ]
    );

    // The recorded span's byte offset points at the `$` of the declaration.
    let inner = run
        .evaluator
        .variable_values()
        .values()
        .find(|v| v.name == "inner")
        .unwrap();
    assert!(src[inner.span.offset as usize..].starts_with("$inner: 2px;"));
}
