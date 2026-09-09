//! `sasso` — a pure-Rust SCSS → CSS compiler.
//!
//! A small, zero-dependency, embeddable Sass engine aiming at byte-exact
//! parity with **current** dart-sass on the subset it implements (e.g.
//! computed colors serialize as `rgb(25%, 50%, 75%)`, not rounded
//! hex). It is sandbox-friendly: `@import` resolution goes through a
//! caller-supplied [`Importer`], so an embedder controls all file access.
//!
//! # Example
//!
//! ```
//! use sasso::{compile, Options};
//!
//! let css = compile("$c: #333; a { color: $c; &:hover { color: $c; } }", &Options::default()).unwrap();
//! assert!(css.contains("a {"));
//! assert!(css.contains("a:hover {"));
//! ```
//!
//! ## Scope
//!
//! This covers a large slice of Sass: variables (`!default`/`!global`),
//! nesting and the `&` parent selector, `#{}` interpolation, `//` and
//! `/* */` comments, unit arithmetic, the color functions, control flow,
//! mixins/functions, `@extend`, `@import`, and the `@use`/`@forward` module
//! system. Both input syntaxes are supported — the brace/semicolon SCSS
//! syntax and the indented `.sass` syntax (selected via [`Options::with_syntax`]
//! or, in the CLI, the input file's extension) — parsing into the same AST and
//! sharing the evaluator and emitter. The north-star target is 100% of the
//! official `sass-spec` suite, tracked by the harness in `spec/`.
//!
//! ## Source positions
//!
//! One rule, no exceptions: **every line and column in this crate is 1-based,
//! and every byte offset is 0-based.** `0` is therefore free to mean "unknown"
//! for a line or column, which is how [`Pos::NONE`], [`Error::line`],
//! [`VarSpan`] (`file == 0`) and the AST's span metadata all spell it — an
//! unknown byte offset is spelled by the accompanying line/file being `0`,
//! since `0` is a perfectly real offset. Columns count **characters**, not
//! bytes; offsets and span lengths count bytes, matching dart-sass.
//!
//! The Source Map v3 format is 0-based on the wire, so `sourcemap.rs` lowers
//! positions at the moment it records them and nowhere else.

// The library's `unsafe` is confined to one audited module — `arena`, the
// scoped bump allocator (perf #5), verified by unit tests + Miri. Every other
// module is `deny(unsafe_code)` (see Cargo.toml `[lints]`); `arena` is the only
// `#[allow]`. The wasm wrapper (`/wasm`, a separate crate) has its own FFI unsafe.
mod arena;

mod builtins;
mod deprecation;
mod diag;
pub mod emit;
mod error;
pub mod eval;
pub mod fxhash;
mod host_fn;
mod importer;
mod musl_math;
mod ryu;
mod lines;
mod source;
pub mod selector;
// Source Map v3 generation: the encoding primitives + JSON model (Phase A),
// wired into emit (Phase B/C) and surfaced through `compile_with_source_map`.
mod sourcemap;
pub mod value;

/// This compiler's version, as in `Cargo.toml`.
///
/// Exposed so a wrapper can report the version of the compiler it actually
/// bundles rather than its own: the FFI crate (`ffi/`) is versioned separately,
/// and its `sasso_version()` used to return `CARGO_PKG_VERSION`, i.e. the
/// wrapper's number. Reading it from here cannot drift.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use arena::{set_arena_bytes, ScopedAlloc};
pub use error::Error;
pub use host_fn::{host_value_op, HostFunction};
pub use importer::{
    CanonicalUrl, CanonicalizeContext, FsImporter, Importer, ImporterError, ImporterResult, MemoryImporter,
};
pub use sourcemap::SourceMap;

// The intermediate representations the staged [`Compiler`] API hands out, at
// the crate root so an embedder that inspects or rewrites them does not have to
// know which module they live in.
/// The parser's own syntax tree, re-exported: sasso evaluates
/// `oxc-css-parser`'s nodes directly rather than lowering them into a
/// tree of its own, so this is the type [`Compiler::parse`] hands back.
pub use oxc_css_parser::ast;
pub use oxc_css_parser::ast::Stylesheet;
/// The bump arena a staged compile parses into. Every tree and source
/// text a [`Compiler`] produces lives here, so it must outlive them.
pub use oxc_css_parser::Allocator;
pub use eval::{EvalOptions, Evaluator, OutItem, OutNode, RuleSelectors, VarSpan, VariableValue};
pub use lines::Pos;
pub use value::Value;

/// Output formatting style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputStyle {
    /// Human-readable, indented output (the default).
    #[default]
    Expanded,
    /// Minified, single-line output.
    Compressed,
}

/// The input syntax flavour.
///
/// Both flavours parse into the same AST and share the evaluator and emitter;
/// only the *block structure* differs (`{}`/`;` for SCSS, indentation +
/// newlines for the indented `.sass` syntax).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Syntax {
    /// The brace/semicolon SCSS syntax (the default).
    #[default]
    Scss,
    /// Plain CSS (a `.css` file loaded via `@use`/`@forward`): the brace/semicolon
    /// grammar, but Sass features are rejected, nesting is preserved verbatim,
    /// and values are emitted without SassScript evaluation.
    Css,
}

/// Compilation options.
pub struct Options<'a> {
    /// Output style.
    pub style: OutputStyle,
    /// Input syntax (SCSS or indented `.sass`).
    pub syntax: Syntax,
    /// Importer used to resolve `@import`; `None` disables file imports.
    pub importer: Option<&'a dyn Importer>,
    /// The input's path/URL as it should appear in diagnostics (e.g.
    /// `input.scss`). `None` disables byte-exact diagnostic snippets (errors
    /// then render as the legacy `Error: <msg> (line:col)` one-liner).
    pub url: Option<&'a str>,
    /// Whether to draw diagnostic snippets with Unicode box-drawing glyphs
    /// (`true`, the default) or the ASCII fallback (`false`, dart's
    /// `--no-unicode`).
    pub unicode: bool,
    /// Whether [`compile_with_source_map`] populates the source map's
    /// `sourcesContent` field with the full text of each source (dart-sass
    /// `--embed-sources`). Default `false` (the map references sources by URL
    /// only). Ignored by the plain [`compile`] path.
    pub source_map_include_sources: bool,
    /// Host-defined custom functions (dart-sass `functions`), registered via
    /// [`Options::with_function`]. Consulted after user `@function`s and module
    /// members but before built-in global functions.
    pub(crate) functions: Vec<host_fn::HostFn>,
    /// Diagnostic handler (dart-sass `logger`). When set, every `@warn`/`@debug`/
    /// deprecation warning is delivered here instead of printed to stderr.
    pub(crate) warn: Option<WarnHandler>,
    /// Emit a `@charset "UTF-8";` (expanded) / U+FEFF BOM (compressed) prefix
    /// when the output contains non-ASCII (dart-sass `charset`, default `true`).
    /// `false` suppresses it.
    pub charset: bool,
    /// Record the evaluated value of every variable declaration as it is
    /// visited, keyed by the declaration's span. See
    /// [`Options::record_variable_values`]. Default `false`.
    pub record_variable_values: bool,
}

/// The kind of a diagnostic delivered to a [`WarnHandler`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarnKind {
    /// A `@warn` directive (or a deprecation warning).
    Warn,
    /// A `@debug` directive.
    Debug,
}

/// A `@warn` / `@debug` / deprecation diagnostic delivered to an embedder's
/// [`WarnHandler`] (dart-sass `logger`).
pub struct WarnEvent<'a> {
    /// Warning vs debug.
    pub kind: WarnKind,
    /// True for a deprecation warning.
    pub deprecation: bool,
    /// The deprecation id (e.g. `"slash-div"`), or `""` when not a deprecation.
    pub deprecation_id: &'a str,
    /// The raw message text (the `@warn`/`@debug` value, or deprecation message).
    pub message: &'a str,
    /// The full dart-style block sasso would otherwise print to stderr (header +
    /// snippet + stack trace), for a faithful default logger.
    pub formatted: &'a str,
    /// The source URL for the diagnostic's span; `""` when not available.
    pub url: &'a str,
    /// The 1-based line for the diagnostic's span; `0` when not available.
    pub line: usize,
}

/// An embedder's diagnostic handler (dart-sass `logger`). Receives every
/// `@warn` / `@debug` / deprecation warning; if unset, they print to stderr.
pub type WarnHandler = std::rc::Rc<dyn Fn(&WarnEvent<'_>)>;

impl Default for Options<'_> {
    fn default() -> Self {
        Options {
            style: OutputStyle::default(),
            syntax: Syntax::default(),
            importer: None,
            url: None,
            unicode: true,
            source_map_include_sources: false,
            functions: Vec::new(),
            warn: None,
            charset: true,
            record_variable_values: false,
        }
    }
}

impl<'a> Options<'a> {
    /// Create default options (expanded, SCSS, no importer).
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: set the output style.
    #[must_use]
    pub fn with_style(mut self, style: OutputStyle) -> Self {
        self.style = style;
        self
    }

    /// Builder: set the input syntax.
    #[must_use]
    pub fn with_syntax(mut self, syntax: Syntax) -> Self {
        self.syntax = syntax;
        self
    }

    /// Builder: set the importer.
    #[must_use]
    pub fn with_importer(mut self, importer: &'a dyn Importer) -> Self {
        self.importer = Some(importer);
        self
    }

    /// Builder: set the diagnostic display URL (enables byte-exact snippets).
    #[must_use]
    pub fn with_url(mut self, url: &'a str) -> Self {
        self.url = Some(url);
        self
    }

    /// Builder: select the diagnostic glyph set (`false` = ASCII / `--no-unicode`).
    #[must_use]
    pub fn with_unicode(mut self, unicode: bool) -> Self {
        self.unicode = unicode;
        self
    }

    /// Builder: whether [`compile_with_source_map`] embeds each source's full
    /// text in the map's `sourcesContent` (default `false`).
    #[must_use]
    pub fn with_source_map_include_sources(mut self, include: bool) -> Self {
        self.source_map_include_sources = include;
        self
    }

    /// Register a host-defined custom function (dart-sass `functions`).
    ///
    /// `signature` is a Sass function signature — a name and parameter list,
    /// e.g. `"pow($base, $exponent)"` or `"to-list($args...)"`. `callback`
    /// receives the bound arguments serialized to sasso's host-value wire format
    /// and returns the result serialized the same way (or an `Err(message)` that
    /// becomes a compile error). This byte-oriented boundary lets embedders
    /// (wasm/FFI) bridge to their own value system without exposing sasso's
    /// internal `Value` type.
    ///
    /// Custom functions take precedence over built-in global functions but not
    /// over user `@function` definitions or `@use`d module members. A malformed
    /// signature is reported only if the function is actually called.
    #[must_use]
    pub fn with_function(mut self, signature: &str, callback: host_fn::HostFunction) -> Self {
        let (name, params) = host_fn::parse_signature(signature);
        self.functions.push(host_fn::HostFn {
            name,
            params,
            callback,
        });
        self
    }

    /// Set the diagnostic handler (dart-sass `logger`). Every `@warn`/`@debug`/
    /// deprecation warning is delivered to `handler` instead of being printed to
    /// stderr (the default when unset).
    #[must_use]
    pub fn with_warn_handler(mut self, handler: WarnHandler) -> Self {
        self.warn = Some(handler);
        self
    }

    /// Set whether to emit the `@charset`/BOM prefix for non-ASCII output
    /// (dart-sass `charset`, default `true`).
    #[must_use]
    pub fn with_charset(mut self, charset: bool) -> Self {
        self.charset = charset;
        self
    }

    /// Record the evaluated value of every variable declaration as it is
    /// visited, keyed by the declaration's span.
    ///
    /// Variables declared inside a nested rule are scoped to that rule and no
    /// longer exist in the environment once evaluation finishes, so looking them
    /// up afterwards with [`crate::Evaluator::env`] reports them as undefined.
    /// When this flag is set, each declaration's name and evaluated value are
    /// captured at the moment they are computed — while still in scope — and
    /// retained in [`crate::Evaluator::variable_values`] for inspection after
    /// compilation. This is intended for tooling (symbol collection, language
    /// servers, etc.) rather than normal compilation.
    ///
    /// The record lives on the [`Evaluator`], which the one-shot [`compile`] /
    /// [`compile_with_source_map`] entry points drop; reach it through the
    /// staged [`Compiler`] API:
    ///
    /// ```
    /// use sasso::{Compiler, MemoryImporter, Options};
    ///
    /// let files = MemoryImporter::new().with_file("in.scss", "a { $inner: 3px; width: $inner; }");
    /// let options = Options::default()
    ///     .record_variable_values(true)
    ///     .with_importer(&files);
    /// let compiler = Compiler::new("in.scss", &options).unwrap();
    /// let sheet = compiler.parse().unwrap();
    /// let run = compiler.evaluate(&sheet).unwrap();
    /// let names: Vec<&str> = run
    ///     .evaluator
    ///     .variable_values()
    ///     .values()
    ///     .map(|v| v.name.as_str())
    ///     .collect();
    /// assert_eq!(names, ["inner"]);
    /// // …and the rule-scoped `$inner` is not in the environment any more.
    /// assert!(run.evaluator.variable("inner").is_none());
    /// ```
    ///
    /// By default this is `false` and no values are recorded.
    #[must_use]
    #[inline]
    pub const fn record_variable_values(mut self, record: bool) -> Self {
        self.record_variable_values = record;
        self
    }

    /// Builder alias for [`Options::record_variable_values`], for callers that
    /// prefer the crate's `with_*` builder spelling.
    #[must_use]
    #[inline]
    pub const fn with_record_variable_values(self, record: bool) -> Self {
        self.record_variable_values(record)
    }
}

/// Compile SCSS source to CSS.
///
/// # Errors
///
/// Returns [`Error`] on a parse or evaluation failure (with a 1-based
/// source position when known).
///
/// # Allocator scope
///
/// When the binary installs [`ScopedAlloc`] as its `#[global_allocator]`, this
/// function brackets the whole compile in a bump-arena scope: every allocation
/// `compile_inner` makes is a pointer bump from a per-thread arena that is freed
/// wholesale when the scope ends. The returned `Result` is allocated *in* the
/// arena, so it is deep-cloned out to the system allocator *before* the arena is
/// reset — the value handed back to the caller never points into the arena. When
/// no `ScopedAlloc` is installed the scope primitives are inert (depth tracking
/// only) and every allocation goes to the system allocator as usual, so this
/// wrapper is correct (just with a redundant clone) under any global allocator.
pub fn compile(source: &str, options: &Options<'_>) -> Result<String, Error> {
    // Enter the arena scope. The RAII guard's `Drop` leaves + resets the arena
    // on the *panic* path; the success path below finishes manually and forgets
    // the guard, so there is no double-leave.
    let guard = arena::Scope::enter();
    // All allocations here bump from the arena (when ScopedAlloc is installed).
    let result = compile_inner(source, options);
    // Leave the scope WITHOUT resetting yet: depth drops to 0, so the arena is
    // now inactive and subsequent allocations route to the system allocator —
    // but the arena memory is still intact and `result` may point into it.
    let outermost = arena::leave_no_reset();
    // Deep-clone the result to the system allocator while the scope is inactive.
    // `Error` derives `Clone`, so both the `Ok(String)` and `Err(message)` cases
    // are copied out byte-for-byte to system-owned memory.
    let owned = result.clone();
    // Drop the arena-resident original (in-arena `dealloc` is a no-op) before
    // the region it lives in is reclaimed.
    drop(result);
    // Only the outermost scope owns the arena's lifetime; reset frees it all.
    if outermost {
        arena::reset();
    }
    // We finished the scope manually; suppress the guard's `Drop` to avoid a
    // second leave/reset.
    std::mem::forget(guard);
    owned
}

/// The CSS plus its source map, returned by [`compile_with_source_map`].
#[derive(Clone, Debug)]
pub struct CompileResult {
    /// The compiled CSS (identical to what [`compile`] would return for the
    /// same `source`/`options` — the map is generated alongside, not instead).
    pub css: String,
    /// The Source Map v3 describing `css`. Serialize it with
    /// [`SourceMap::to_json`].
    pub source_map: SourceMap,
}

/// Compile SCSS source to CSS *and* a [Source Map v3](SourceMap).
///
/// The `css` field is byte-for-byte what [`compile`] returns; the map is built
/// alongside it. The map's `file` is the basename of [`Options::url`] (or
/// `"stdin"` when no URL is set) and its `sources` are the source URLs.
/// [`Options::with_source_map_include_sources`] controls whether each source's
/// full text is embedded in `sourcesContent`.
///
/// Maps the start of each selector, declaration property name, declaration
/// value, at-rule keyword, and comment. A value that is a bare `$name` maps
/// back to where that variable was DEFINED, like dart-sass.
///
/// # Errors
///
/// Returns [`Error`] on a parse or evaluation failure, like [`compile`].
pub fn compile_with_source_map(source: &str, options: &Options<'_>) -> Result<CompileResult, Error> {
    // Mirror `compile`'s arena bracketing so the returned value is deep-cloned
    // out to the system allocator before the arena is reset.
    let guard = arena::Scope::enter();
    let result = compile_inner_sm(source, options);
    let outermost = arena::leave_no_reset();
    let owned = result.clone();
    drop(result);
    if outermost {
        arena::reset();
    }
    std::mem::forget(guard);
    owned
}

/// The basename of a path/URL (everything after the last `/`), used for the
/// source map's `file` field.
fn basename(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

/// A staged compile of one **entry file**, exposing every intermediate form of
/// the pipeline.
///
/// [`compile`] takes a source string and runs parse → evaluate → emit as one
/// closed step. `Compiler` instead names an entry *file*, loads it through
/// [`Options::importer`] — the same virtual filesystem `@use`/`@forward`/
/// `@import` resolve against — and hands each intermediate back so it can be
/// inspected and *modified* before feeding the next one:
///
/// - [`parse`](Self::parse) yields an [`ast::Stylesheet`] — the statement tree,
///   with every node and its selector templates public and mutable.
/// - [`evaluate`](Self::evaluate) yields an [`Evaluation`]: the flattened
///   [`OutNode`] tree (resolved selectors, declarations, at-rules) plus the
///   [`Evaluator`] that produced it, for its environment and — with
///   [`Options::record_variable_values`] — its recorded variable values.
/// - [`emit`](Self::emit) serializes a (possibly rewritten) node slice to CSS.
///
/// Each step is a plain function of its input, so a step can also be skipped or
/// repeated: one node tree can be emitted twice under different styles, and one
/// stylesheet can be evaluated more than once.
///
/// Because every source has a name, every diagnostic, every [`VarSpan`] and
/// every source-map entry is attributed to the file it actually came from —
/// including two same-named partials in different directories, which are
/// distinct [`CanonicalUrl`]s and therefore distinct interned file ids.
///
/// ```
/// use sasso::{Compiler, MemoryImporter, Options, OutNode, RuleSelectors};
///
/// let files = MemoryImporter::new()
///     .with_file(
///         "main.scss",
///         r#"@use "theme"; a { color: theme.$fg; } b { color: blue; }"#,
///     )
///     .with_file("_theme.scss", "$fg: red;");
/// let options = Options::default().with_importer(&files);
/// let compiler = Compiler::new("main.scss", &options).unwrap();
///
/// // Parse, then drop every rule but the `@use` and the first rule.
/// let mut sheet = compiler.parse().unwrap();
/// sheet.stmts.truncate(2);
///
/// // Evaluate, then rewrite the resolved selectors.
/// let mut run = compiler.evaluate(&sheet).unwrap();
/// for node in &mut run.nodes {
///     if let OutNode::Rule { selectors, .. } = node {
///         let scoped = selectors.to_strings().iter().map(|s| format!(".theme {s}")).collect();
///         *selectors = RuleSelectors::Raw(scoped);
///     }
/// }
///
/// assert_eq!(compiler.emit(&run.nodes), ".theme a {\n  color: red;\n}");
/// ```
///
/// # Allocator scope
///
/// Unlike [`compile`], the staged API does not run inside the bump-arena scope
/// (its intermediates outlive any one step), so every allocation goes to the
/// global allocator even when [`ScopedAlloc`] is installed.
pub struct Compiler<'a> {
    /// The bump arena every stage parses into. The parser's trees borrow
    /// from it, and the evaluator holds those borrows for the whole compile,
    /// so it is the caller's to own and must outlive this `Compiler`.
    allocator: &'a Allocator,
    /// The entry file's source text, as the importer loaded it (owned: the
    /// importer hands back a `String` that must outlive every stage).
    source: String,
    /// The entry's canonical URL — its identity for the module cache, for
    /// interned file ids, and the base relative loads inside it resolve
    /// against.
    entry: CanonicalUrl,
    /// The entry's DISPLAY name in diagnostics: the name passed to
    /// [`new`](Self::new), or [`Options::url`] when set.
    url: String,
    /// The syntax the importer reported for the entry file.
    syntax: Syntax,
    options: &'a Options<'a>,
    source_map: bool,
}

/// Names the entry file and how it will be compiled; the source text and the
/// options are elided (`Options` holds caller callbacks and is not `Debug`).
impl std::fmt::Debug for Compiler<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compiler")
            .field("entry", &self.entry.as_str())
            .field("url", &self.url)
            .field("syntax", &self.syntax)
            .field("source_len", &self.source.len())
            .field("source_map", &self.source_map)
            .finish()
    }
}

/// The result of [`Compiler::evaluate`]: the output tree plus the evaluator
/// that produced it.
pub struct Evaluation<'a> {
    /// The flattened output tree, after `@extend` resolution — the same nodes
    /// [`compile`] would serialize. Mutate freely before [`Compiler::emit`].
    pub nodes: Vec<OutNode>,
    /// The evaluator, kept alive past `eval_sheet` so its post-compile state is
    /// reachable: [`Evaluator::variable_values`], [`Evaluator::env`],
    /// [`Evaluator::file_url`], [`Evaluator::source_table`].
    pub evaluator: Evaluator<'a>,
}

/// A parsed entry file: the tree, plus the source context that positions and
/// diagnostics resolve against.
///
/// The tree is the parser's own [`Stylesheet`], borrowed mutably from the
/// arena — rewrite it in place before handing it to
/// [`evaluate`](Compiler::evaluate), which takes it by value because the
/// evaluator keeps the borrow for the rest of the compile.
pub struct Parsed<'a> {
    pub(crate) file: &'a source::SourceFile<'a>,
    /// The parsed statements.
    pub sheet: &'a mut ast::Stylesheet<'a>,
}

impl<'a> Compiler<'a> {
    /// A staged compile of the entry file named `entry`, loaded through
    /// [`Options::importer`].
    ///
    /// `entry` is resolved exactly like a `@use` URL with no containing file:
    /// [`Importer::canonicalize`] maps it to a [`CanonicalUrl`], then
    /// [`Importer::load`] fetches its source and syntax. That canonical URL is
    /// the entry's identity — relative `@use`/`@forward`/`@import` URLs inside
    /// it resolve against it, and spans stamped in it intern under it.
    ///
    /// The name shown in diagnostics is `entry` itself, unless
    /// [`Options::url`] overrides it.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when no importer is configured, when the importer
    /// cannot resolve or load `entry`, or when the importer itself fails.
    pub fn new(
        allocator: &'a Allocator,
        entry: &str,
        options: &'a Options<'a>,
    ) -> Result<Self, Error> {
        let importer = options.importer.ok_or_else(|| {
            Error::unpositioned(
                "No importer configured: the staged compiler loads its entry file through Options::with_importer.",
            )
        })?;
        let ctx = CanonicalizeContext {
            from_import: false,
            containing_url: None,
        };
        let canonical = importer
            .canonicalize(entry, &ctx)
            .map_err(|e| Error::unpositioned(e.message))?
            .ok_or_else(|| Error::unpositioned(format!("Can't find stylesheet {entry:?}.")))?;
        let loaded = importer
            .load(&canonical)
            .map_err(|e| Error::unpositioned(e.message))?
            .ok_or_else(|| Error::unpositioned(format!("Can't load stylesheet {:?}.", canonical.as_str())))?;
        Ok(Compiler {
            allocator,
            source: loaded.contents,
            entry: canonical,
            url: options.url.unwrap_or(entry).to_string(),
            syntax: loaded.syntax,
            options,
            source_map: false,
        })
    }

    /// A staged compile of a bare source string with no entry file — what the
    /// one-shot [`compile`] / [`compile_with_source_map`] run internally.
    ///
    /// Kept private: the public staged API is file-based so that every source
    /// it touches has a name to attribute spans and diagnostics to.
    pub(crate) fn from_source(
        allocator: &'a Allocator,
        source: &'a str,
        options: &'a Options<'a>,
    ) -> Self {
        Compiler {
            allocator,
            source: source.to_string(),
            // No importer resolved this, so its identity is its display name
            // (`""` when none was configured) — there is only ever one such
            // file, so it cannot collide.
            entry: CanonicalUrl::new(options.url.unwrap_or_default()),
            url: options.url.unwrap_or_default().to_string(),
            syntax: options.syntax,
            options,
            source_map: false,
        }
    }

    /// The entry file's canonical URL, as the importer resolved it.
    pub fn entry(&self) -> &CanonicalUrl {
        &self.entry
    }

    /// The name the entry file appears under in diagnostics.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The entry file's source text, as the importer loaded it.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The syntax the importer reported for the entry file.
    pub fn syntax(&self) -> Syntax {
        self.syntax
    }

    /// Enable the evaluator's source-map bookkeeping (definition spans for
    /// variables, value spans on declarations), required by
    /// [`emit_with_source_map`](Self::emit_with_source_map). Off by default,
    /// exactly like [`compile`].
    #[must_use]
    pub fn with_source_map(mut self, source_map: bool) -> Self {
        self.source_map = source_map;
        self
    }

    /// Parse the source into a stylesheet, without evaluating it.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on a syntax error, carrying a rendered diagnostic
    /// snippet naming the entry file.
    pub fn parse(&self) -> Result<Parsed<'a>, Error> {
        let syntax = match self.syntax {
            Syntax::Scss => oxc_css_parser::Syntax::Scss,
            Syntax::Css => oxc_css_parser::Syntax::Css,
        };
        let url = if self.url.is_empty() {
            self.entry.as_str()
        } else {
            &self.url
        };
        // A parse error never reached the evaluator, so render its snippet here
        // (single `root stylesheet` frame) when a diagnostic URL is known.
        let (file, sheet) = source::SourceFile::parse(self.allocator, url, &self.source, syntax)
            .map_err(|e| self.render_entry_error(e))?;
        Ok(Parsed { file, sheet })
    }

    /// Render an error raised against the ENTRY file before (or without) an
    /// evaluator — a parse error, or a declaration-placement violation — into
    /// the same block the evaluator produces: header, snippet, and a single
    /// `root stylesheet` frame. Nothing has been loaded at that point, so the
    /// position can only be in the entry file.
    fn render_entry_error(&self, mut e: Error) -> Error {
        if self.url.is_empty() || !e.has_position() {
            return e;
        }
        e = e.with_file(self.url.clone());
        if e.rendered.is_none() {
            let span = diag::Span {
                line: e.line,
                col: e.col,
                length: e.length,
            };
            e.rendered = Some(diag::render_error(
                &e.message,
                &self.source,
                &self.url,
                span,
                self.glyphs(),
            ));
        }
        e
    }

    /// A fresh evaluator configured from these options — the same one
    /// [`evaluate`](Self::evaluate) builds. Use it directly to drive
    /// [`Evaluator::eval_sheet`] yourself.
    pub fn evaluator(&self, parsed: &Parsed<'a>) -> Evaluator<'a> {
        // A file-based compile always has a name for every source, so it always
        // renders byte-exact `Error:`/`WARNING:` blocks. The nameless
        // `from_source` path keeps the old rule: diagnostics only when the
        // caller supplied a display URL, and the source-map run labels an
        // unnamed entry `stdin` (so `sourcesContent` still works).
        let named = !self.url.is_empty();
        // `EvalOptions` borrows for `'a`, so the strings it points at are
        // interned into the arena rather than borrowed from `self`.
        let source: &'a str = self.allocator.alloc_str(&self.source);
        let url: &'a str = self.allocator.alloc_str(&self.url);
        let entry_key: &'a str = self.allocator.alloc_str(self.entry.as_str());
        let (source, url) = match (self.source_map, named) {
            (true, true) | (false, true) => (source, url),
            (true, false) => (source, "stdin"),
            (false, false) => ("", ""),
        };
        Evaluator::new(self.allocator, parsed.file, eval::EvalOptions {
            style: self.options.style,
            importer: self.options.importer,
            functions: &self.options.functions,
            source,
            diagnostics: named,
            url,
            entry_key,
            glyphs: self.glyphs(),
            warn: self.options.warn.as_ref(),
            source_map: self.source_map,
            record_variable_values: self.options.record_variable_values,
        })
    }

    /// Evaluate a stylesheet — the one [`parse`](Self::parse) returned, or one
    /// modified since — into the output tree, and hand back the evaluator with
    /// it.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on a declaration-placement violation (an `@function` /
    /// `@mixin` in a control directive, `@content` outside a mixin, …) or any
    /// evaluation failure.
    pub fn evaluate(&self, parsed: Parsed<'a>) -> Result<Evaluation<'a>, Error> {
        let mut evaluator = self.evaluator(&parsed);
        // Re-borrowing the owned `&'a mut` as shared keeps the arena lifetime,
        // which is why `parsed` is taken by value: the evaluator holds the tree
        // for as long as it lives.
        let Parsed { file, sheet } = parsed;
        let sheet: &'a ast::Stylesheet<'a> = sheet;
        // Reject `@function`/`@mixin` declarations in control directives or
        // function/mixin bodies (a compile-time restriction, checked before
        // eval) — checked here rather than in `parse` so a caller-modified tree
        // is validated too.
        eval::validate_declarations(file, sheet).map_err(|e| self.render_entry_error(e))?;
        let mut nodes = Vec::new();
        evaluator.eval_sheet(sheet, &mut nodes)?;
        Ok(Evaluation { nodes, evaluator })
    }

    /// Serialize an output tree to CSS, honouring [`Options::style`] and
    /// [`Options::charset`].
    pub fn emit(&self, nodes: &[OutNode]) -> String {
        emit::emit(nodes, self.options.style, self.options.charset)
    }

    /// Serialize an output tree to CSS *and* a [Source Map v3](SourceMap).
    ///
    /// `evaluator` must be the one that produced `nodes` — its interned file
    /// table becomes the map's `sources`. Build it from a [`Compiler`] with
    /// [`with_source_map(true)`](Self::with_source_map); without that flag the
    /// evaluator records no spans and the map has no mappings.
    pub fn emit_with_source_map(&self, nodes: &[OutNode], evaluator: &mut Evaluator<'_>) -> CompileResult {
        // The entry name labels the entry source in the map
        // (`file`/`sources[0]`); it is also the evaluator's `current_url`, so
        // every entry-file node is stamped with a non-zero file id.
        let entry_name = if self.entry.as_str().is_empty() {
            "stdin"
        } else {
            self.entry.as_str()
        };
        let (css, body_off, collector) = emit::emit_with_map(nodes, self.options.style, self.options.charset);
        let mappings = collector.finalize(&css, body_off).encode();
        let (sources, sources_content) =
            evaluator.source_table(entry_name, self.options.source_map_include_sources);
        CompileResult {
            css,
            source_map: SourceMap {
                file: Some(basename(entry_name).to_string()),
                sources,
                sources_content,
                mappings,
            },
        }
    }

    /// Run the whole pipeline: parse, evaluate, emit.
    ///
    /// Byte-for-byte what [`compile`] returns for the same source and options
    /// (the free function additionally brackets the run in an arena scope).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on a parse or evaluation failure.
    pub fn compile(&self) -> Result<String, Error> {
        let parsed = self.parse()?;
        let run = self.evaluate(parsed)?;
        Ok(self.emit(&run.nodes))
    }

    /// The glyph set for diagnostic snippets (ASCII under `--no-unicode`).
    fn glyphs(&self) -> diag::GlyphSet {
        if self.options.unicode {
            diag::GlyphSet::Unicode
        } else {
            diag::GlyphSet::Ascii
        }
    }
}

/// The source-map compile pipeline: parse + evaluate exactly like
/// [`compile_inner`], then emit with the source-map collector and assemble the
/// [`SourceMap`].
fn compile_inner_sm(source: &str, options: &Options<'_>) -> Result<CompileResult, Error> {
    let arena = Allocator::default();
    let compiler = Compiler::from_source(&arena, source, options).with_source_map(true);
    let parsed = compiler.parse()?;
    let mut run = compiler.evaluate(parsed)?;
    Ok(compiler.emit_with_source_map(&run.nodes, &mut run.evaluator))
}

/// The actual compile pipeline. Runs inside the arena scope established by
/// [`compile`]; all of its allocations may be arena-resident, so its result is
/// copied out by the wrapper before the arena is reset.
fn compile_inner(source: &str, options: &Options<'_>) -> Result<String, Error> {
    let arena = Allocator::default();
    Compiler::from_source(&arena, source, options).compile()
}
