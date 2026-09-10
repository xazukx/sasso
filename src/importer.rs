//! The `Importer` trait plus the built-in filesystem importer (`FsImporter`)
//! and its dart-faithful partial / index / import-only resolver.

use std::path::{Path, PathBuf};

use crate::Syntax;

/// An opaque, importer-defined canonical identifier for a resolved stylesheet.
///
/// Two URLs that canonicalize to the same `CanonicalUrl` are the SAME loaded
/// file — it is the module-cache / dedup key. What "canonical" *means* is the
/// importer's to define; sasso only uses it as an `Eq`/`Hash` key. For
/// [`FsImporter`] it is the absolute filesystem path. The field is private so
/// constraints/normalization can be added later without an API break.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CanonicalUrl(String);

impl CanonicalUrl {
    /// Wrap an importer-chosen canonical identifier.
    pub fn new(url: impl Into<String>) -> Self {
        CanonicalUrl(url.into())
    }

    /// The underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The source an [`Importer::load`] produced for a [`CanonicalUrl`] — dart-sass's
/// `ImporterResult`.
#[derive(Clone, Debug)]
pub struct ImporterResult {
    /// The stylesheet source text.
    pub contents: String,
    /// The syntax `contents` should be parsed with (SCSS / indented / plain CSS).
    pub syntax: Syntax,
    /// The URL to record for this source in generated source maps (dart-sass
    /// `ImporterResult.sourceMapUrl`); `None` falls back to the canonical URL.
    pub source_map_url: Option<String>,
}

/// A real importer failure — an I/O error, a permission error, an ambiguous
/// match, an invalid URL, … — as distinct from a plain miss. Surfaced as an
/// actionable compile [`crate::Error`], never silently treated as "not found".
#[derive(Clone, Debug)]
pub struct ImporterError {
    /// Human-readable description, used as the compile error message.
    pub message: String,
}

/// Context passed to [`Importer::canonicalize`] (dart-sass's `CanonicalizeContext`).
pub struct CanonicalizeContext<'a> {
    /// `true` when resolving an `@import` (which additionally considers
    /// import-only `*.import.{scss,sass}` files), `false` for `@use`/`@forward`.
    pub from_import: bool,
    /// The canonical URL of the stylesheet containing the rule being resolved,
    /// if any (`None` only when there is no containing file). Relative URLs
    /// resolve against it.
    pub containing_url: Option<&'a CanonicalUrl>,
}

/// Resolves `@use` / `@forward` / `@import` URLs in dart-sass's two phases.
///
/// [`canonicalize`](Importer::canonicalize) maps a (possibly relative,
/// extension-less) URL to a stable canonical identity WITHOUT loading it (the
/// result is the module-cache key); [`load`](Importer::load) then fetches that
/// identity's source. Both methods distinguish three outcomes: `Ok(Some(_))` =
/// handled; `Ok(None)` = this importer doesn't handle the URL; `Err(_)` =
/// handled but failed (a real, reportable error).
///
/// Implementing this gives the caller full control over where partials come
/// from — and, crucially, keeps all file access on the caller's side of a
/// sandbox boundary.
pub trait Importer {
    /// Map `url` to its canonical identity, or `Ok(None)` if this importer
    /// cannot resolve it. MUST NOT load the file.
    fn canonicalize(
        &self,
        url: &str,
        ctx: &CanonicalizeContext<'_>,
    ) -> Result<Option<CanonicalUrl>, ImporterError>;

    /// Load the source for a [`CanonicalUrl`] previously returned by
    /// [`canonicalize`](Importer::canonicalize); `Ok(None)` if it can no longer
    /// be found.
    fn load(&self, canonical: &CanonicalUrl) -> Result<Option<ImporterResult>, ImporterError>;
}

/// An in-memory [`Importer`]: a virtual filesystem of `name -> source`, with no
/// disk access at all.
///
/// This is the importer the staged [`crate::Compiler`] is built around — it
/// gives every stylesheet a *name*, so diagnostics, [`crate::VarSpan`]s and
/// source-map entries are attributed to the file each construct actually came
/// from. Two same-named partials in different directories
/// (`theme/_colors.scss`, `base/_colors.scss`) are distinct
/// [`CanonicalUrl`]s and stay distinct all the way through.
///
/// ```
/// use sasso::{Allocator, Compiler, MemoryImporter, Options};
///
/// let files = MemoryImporter::new()
///     .with_file("main.scss", r#"@use "ui/button";"#)
///     .with_file("ui/_button.scss", ".btn { color: red; }");
/// let arena = Allocator::default();
/// let options = Options::default().with_importer(&files);
/// let css = Compiler::new(&arena, "main.scss", &options).unwrap().compile().unwrap();
/// assert_eq!(css, ".btn {\n  color: red;\n}");
/// ```
///
/// # Resolution
///
/// A URL is resolved lexically (there is no directory tree, only exact names)
/// against the containing file's directory first, then against the virtual
/// root — the root acts as an implicit load path. Within each base, the
/// dart-sass candidate order applies: the import-only file (`@import` only),
/// then the partial and the plain file, then the index file. Extensions tried
/// are `.scss` and `.css`, plus the name exactly as written. The syntax of a
/// resolved file comes from its extension (`.css` is plain CSS, anything else
/// is SCSS) unless [`with_file_syntax`](Self::with_file_syntax) overrode it.
///
/// The first candidate that exists wins; unlike dart-sass, an ambiguous pair is
/// not reported as an error.
#[derive(Clone, Debug, Default)]
pub struct MemoryImporter {
    files: std::collections::HashMap<String, (String, Option<Syntax>)>,
}

impl MemoryImporter {
    /// An empty virtual filesystem.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: add `name` with `contents`, its syntax taken from the name's
    /// extension.
    #[must_use]
    pub fn with_file(mut self, name: impl Into<String>, contents: impl Into<String>) -> Self {
        self.insert(name, contents);
        self
    }

    /// Builder: add `name` with `contents`, parsed as `syntax` regardless of
    /// the name's extension.
    #[must_use]
    pub fn with_file_syntax(
        mut self,
        name: impl Into<String>,
        contents: impl Into<String>,
        syntax: Syntax,
    ) -> Self {
        self.files
            .insert(normalize_url(&name.into()), (contents.into(), Some(syntax)));
        self
    }

    /// Add or replace `name`, its syntax taken from the name's extension.
    pub fn insert(&mut self, name: impl Into<String>, contents: impl Into<String>) {
        self.files
            .insert(normalize_url(&name.into()), (contents.into(), None));
    }

    /// Remove `name`, returning its contents if it was present.
    pub fn remove(&mut self, name: &str) -> Option<String> {
        self.files.remove(&normalize_url(name)).map(|(c, _)| c)
    }

    /// Whether `name` exists verbatim (no partial/index resolution).
    pub fn contains(&self, name: &str) -> bool {
        self.files.contains_key(&normalize_url(name))
    }

    /// Every file name in the virtual filesystem, in no particular order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// The candidate names for `url` under `base`, in dart-sass precedence
    /// order. `base` is `""` (the virtual root) or a directory with no trailing
    /// slash.
    fn candidates(base: &str, url: &str, from_import: bool) -> Vec<String> {
        let joined = if base.is_empty() {
            normalize_url(url)
        } else {
            normalize_url(&format!("{base}/{url}"))
        };
        let (dir, file) = match joined.rsplit_once('/') {
            Some((d, f)) => (format!("{d}/"), f.to_string()),
            None => (String::new(), joined.clone()),
        };
        let mut out = Vec::with_capacity(10);
        // 1. Import-only override, for `@import` only.
        if from_import {
            out.push(format!("{dir}_{file}.import.scss"));
            out.push(format!("{dir}{file}.import.scss"));
        }
        // 2. Partial, then plain, per extension; then the name exactly as
        //    written (so `@use "ui/button.scss"` and an entry named
        //    `main.scss` both resolve).
        for ext in ["scss", "css"] {
            out.push(format!("{dir}_{file}.{ext}"));
            out.push(format!("{dir}{file}.{ext}"));
        }
        out.push(joined.clone());
        // 3. Index files, in a directory named after the URL.
        if from_import {
            out.push(format!("{joined}/_index.import.scss"));
            out.push(format!("{joined}/index.import.scss"));
        }
        for ext in ["scss", "css"] {
            out.push(format!("{joined}/_index.{ext}"));
            out.push(format!("{joined}/index.{ext}"));
        }
        out
    }
}

impl Importer for MemoryImporter {
    fn canonicalize(
        &self,
        url: &str,
        ctx: &CanonicalizeContext<'_>,
    ) -> Result<Option<CanonicalUrl>, ImporterError> {
        // The containing file's directory first, then the virtual root (an
        // implicit load path), mirroring `FsImporter`'s base ordering.
        let containing_dir = ctx
            .containing_url
            .and_then(|c| c.as_str().rsplit_once('/'))
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        let mut bases = vec![containing_dir.clone()];
        if !containing_dir.is_empty() {
            bases.push(String::new());
        }
        for base in bases {
            for cand in Self::candidates(&base, url, ctx.from_import) {
                if self.files.contains_key(&cand) {
                    return Ok(Some(CanonicalUrl::new(cand)));
                }
            }
        }
        Ok(None)
    }

    fn load(&self, canonical: &CanonicalUrl) -> Result<Option<ImporterResult>, ImporterError> {
        Ok(self
            .files
            .get(canonical.as_str())
            .map(|(contents, syntax)| ImporterResult {
                contents: contents.clone(),
                syntax: syntax.unwrap_or_else(|| syntax_for_name(canonical.as_str())),
                source_map_url: None,
            }))
    }
}

/// Normalize a virtual file name to the form [`MemoryImporter`] keys on:
/// backslashes become `/`, and `.` / `..` segments are resolved lexically, so
/// `ui/../main.scss` and `./main.scss` name the same file as `main.scss`.
fn normalize_url(name: &str) -> String {
    let slashed = name.replace('\\', "/");
    let normalized = lexical_normalize(&slashed);
    if normalized == "." {
        String::new()
    } else {
        normalized
    }
}

/// The syntax a virtual file name implies, from its extension — the in-memory
/// twin of [`syntax_for_path`].
fn syntax_for_name(name: &str) -> Syntax {
    match name.rsplit_once('.') {
        Some((_, ext)) if ext.eq_ignore_ascii_case("css") => Syntax::Css,
        _ => Syntax::Scss,
    }
}

/// A filesystem [`Importer`] resolving Sass partials (`_name.scss`,
/// `name/_index.scss`) against a list of load paths.
///
/// Intended for CLI/standalone use; an embedder inside a sandbox should
/// supply its own [`Importer`] that routes through the sandbox's file
/// capability instead of touching the disk directly.
pub struct FsImporter {
    load_paths: Vec<PathBuf>,
}

impl FsImporter {
    /// Create an importer that searches `load_paths` in order.
    pub fn new(load_paths: Vec<PathBuf>) -> Self {
        FsImporter { load_paths }
    }
}

impl Importer for FsImporter {
    fn canonicalize(
        &self,
        url: &str,
        ctx: &CanonicalizeContext<'_>,
    ) -> Result<Option<CanonicalUrl>, ImporterError> {
        // dart-sass resolves a relative URL against the containing file's
        // directory first, then the configured load paths. A `CanonicalUrl`
        // here IS the absolute fs path, so the containing dir is its parent; a
        // containing URL with no parent (a bare entry name like `input.scss`)
        // means the current directory — reproducing the old base of
        // `dirname_of(url).unwrap_or_default()` (an empty string => CWD).
        //
        // NB: this `parent()`-based dirname must stay consistent with the
        // evaluator's `dirname_of` (eval/mod.rs), which derives
        // `current_file_dir` (the `@import` cache key) from the same canonical
        // URL — both use `Path::parent` + treat an empty parent as CWD.
        let base_dir: PathBuf = match ctx.containing_url {
            Some(c) => match Path::new(c.as_str()).parent() {
                Some(par) if !par.as_os_str().is_empty() => par.to_path_buf(),
                _ => PathBuf::new(),
            },
            None => PathBuf::new(),
        };
        let bases = std::iter::once(base_dir).chain(self.load_paths.iter().cloned());
        for base in bases {
            // `@use`/`@forward` (`from_import == false`) never consider
            // `.import` files (those are an `@import`-only escape hatch).
            match resolve_in_base(&base, url, ctx.from_import) {
                Resolution::Found(p) => {
                    // The canonical key is the resolved absolute path so the
                    // same file loaded via different URLs is cached once.
                    let key = std::fs::canonicalize(&p)
                        .map(|c| c.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| p.to_string_lossy().into_owned());
                    return Ok(Some(CanonicalUrl::new(key)));
                }
                // An ambiguous match is an error in dart-sass; we preserve the
                // existing behavior of surfacing it as a miss (the eval layer
                // turns that into the import error) rather than an `Err`.
                Resolution::Ambiguous => return Ok(None),
                Resolution::NotFound => {}
            }
        }
        Ok(None)
    }

    fn load(&self, canonical: &CanonicalUrl) -> Result<Option<ImporterResult>, ImporterError> {
        let p = Path::new(canonical.as_str());
        match std::fs::read_to_string(p) {
            Ok(contents) => Ok(Some(ImporterResult {
                contents,
                syntax: syntax_for_path(p),
                source_map_url: None,
            })),
            // The file vanished between `canonicalize` and `load` -> a miss.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            // Any other read failure (permission, invalid UTF-8, I/O) is a real,
            // reportable error via the `ImporterError` channel, not a misleading
            // "can't find stylesheet".
            Err(e) => Err(ImporterError {
                message: format!("Cannot read {}: {e}", p.display()),
            }),
        }
    }
}

/// The syntax a resolved file should be parsed with, from its extension: a
/// `.css` file is the CSS syntax, anything else (`.scss`) is SCSS.
fn syntax_for_path(p: &Path) -> Syntax {
    match p.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("css") => Syntax::Css,
        _ => Syntax::Scss,
    }
}

/// Outcome of resolving an `@import` argument within a single load path.
enum Resolution {
    /// Exactly one candidate file matched.
    Found(PathBuf),
    /// Two or more candidates matched at the same precedence tier — dart-sass
    /// treats this as an error ("It's not clear which file to import.").
    Ambiguous,
    /// No candidate matched in this base directory.
    NotFound,
}

/// Resolve `@import "path"` against `base`, following dart-sass precedence.
///
/// dart-sass tries, in strict order (each "tier" is checked together; if a
/// tier has more than one match it is ambiguous, if it has exactly one that
/// wins, otherwise fall through to the next tier):
///
/// 1. import-only, non-index: `name.import.{scss,sass}` + partials
/// 2. normal, non-index:      `name.{scss,sass}` + partials
/// 3. import-only index:      `name/index.import.{…}` + `_index.import.{…}`
/// 4. normal index:           `name/index.{…}` + `_index.{…}`
///
/// An explicit `.scss`/`.sass` extension keeps only the matching extension but
/// still honours the import-only override (`name.import.scss`).
///
/// We deliberately do **not** resolve plain `.css` files here: importing a CSS
/// file as a stylesheet is a distinct feature (with its own strict-CSS parsing
/// rules) that this build doesn't implement, and treating `foo.css` as a
/// stylesheet would mis-handle constructs that dart-sass rejects.
///
/// An import-only file whose body is only `@forward`/`@use` (the real-world
/// shape — re-exporting another module) is treated as unusable and skipped,
/// since this build has no module system; resolution then falls through to the
/// normal file, matching the output we can actually produce.
fn resolve_in_base(base: &Path, path: &str, allow_import_only: bool) -> Resolution {
    // dart-sass normalizes the URL lexically before touching the filesystem,
    // so `foo/bar/../baz` resolves even when `foo/bar` doesn't exist.
    let normalized = lexical_normalize(path);
    let path = normalized.as_str();
    let p = Path::new(path);
    let dir = match p.parent() {
        Some(par) if !par.as_os_str().is_empty() => base.join(par),
        _ => base.to_path_buf(),
    };
    let file = p.file_name().and_then(|s| s.to_str()).unwrap_or(path);

    // Explicit `.css` extension: only the plain-CSS candidate is considered.
    // (An `@import "x.css"` never reaches here — it's a passthrough upstream —
    // but `@use "x.css"` does.)
    if let Some(stem) = file.strip_suffix(".css") {
        return match tier_exact(&dir, stem, &["css"], false) {
            Tier::One(p) => Resolution::Found(p),
            Tier::Many => Resolution::Ambiguous,
            Tier::None => Resolution::NotFound,
        };
    }

    // Explicit `.scss`/`.sass` extension: only that extension is considered.
    let explicit_ext = [".scss", ".sass"]
        .into_iter()
        .find(|ext| file.ends_with(ext));

    if let Some(ext) = explicit_ext {
        let stem = &file[..file.len() - ext.len()];
        // Strip the leading dot so `ext` is e.g. "scss".
        let ext = &ext[1..];
        // Tier 1: import-only override for the explicit extension.
        if allow_import_only {
            match tier_exact(&dir, stem, &[ext], true) {
                Tier::One(p) => return Resolution::Found(p),
                Tier::Many => return Resolution::Ambiguous,
                Tier::None => {}
            }
        }
        // Tier 2: the file as written (+ partial).
        return match tier_exact(&dir, stem, &[ext], false) {
            Tier::One(p) => Resolution::Found(p),
            Tier::Many => Resolution::Ambiguous,
            Tier::None => Resolution::NotFound,
        };
    }

    // Extensionless: scss/sass are equal precedence, css is a fallback.
    let mut non_index: Vec<(&str, bool)> = Vec::with_capacity(2);
    if allow_import_only {
        non_index.push((file, true));
    }
    non_index.push((file, false));
    for (stem, import_only) in &non_index {
        match tier_with_extensions(&dir, stem, *import_only) {
            Tier::One(p) => return Resolution::Found(p),
            Tier::Many => return Resolution::Ambiguous,
            Tier::None => {}
        }
    }

    // A plain `.css` file (loaded in plain-CSS mode): after the Sass
    // candidates, but BEFORE index files (dart-sass `_tryPathWithExtensions`
    // tries `$path.css` before `_tryPathAsDirectory`).
    match tier_exact(&dir, file, &["css"], false) {
        Tier::One(p) => return Resolution::Found(p),
        Tier::Many => return Resolution::Ambiguous,
        Tier::None => {}
    }

    // Index files live in a subdirectory named after the import path.
    let index_dir = dir.join(file);
    let index_modes: &[bool] = if allow_import_only {
        &[true, false]
    } else {
        &[false]
    };
    for import_only in index_modes {
        match tier_with_extensions(&index_dir, "index", *import_only) {
            Tier::One(p) => return Resolution::Found(p),
            Tier::Many => return Resolution::Ambiguous,
            Tier::None => {}
        }
    }

    Resolution::NotFound
}

/// Lexically remove `.` and `..` segments from a URL path (no filesystem
/// access; leading `..` segments that would escape are kept).
fn lexical_normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(&s) if s != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    let mut s = out.join("/");
    if path.starts_with('/') {
        s.insert(0, '/');
    }
    if s.is_empty() {
        s.push('.');
    }
    s
}

/// One precedence tier's matches.
enum Tier {
    None,
    One(PathBuf),
    Many,
}

impl Tier {
    fn from(mut found: Vec<PathBuf>) -> Tier {
        match found.len() {
            0 => Tier::None,
            1 => Tier::One(found.pop().unwrap_or_default()),
            _ => Tier::Many,
        }
    }
}

/// Try a tier with the standard extension grouping: `scss` and `sass` are
/// checked together (any match there wins the tier). Each extension is checked
/// in both non-partial and partial (`_name`) forms.
fn tier_with_extensions(dir: &Path, stem: &str, import_only: bool) -> Tier {
    tier_exact(dir, stem, &["scss", "sass"], import_only)
}

/// Collect existing candidate files for `stem` under `dir` across `exts`, in
/// non-partial and partial forms, optionally inserting the `.import` suffix
/// (import-only files). Returns how many matched.
fn tier_exact(dir: &Path, stem: &str, exts: &[&str], import_only: bool) -> Tier {
    let mut found = Vec::new();
    let suffix = if import_only { ".import" } else { "" };
    for ext in exts {
        for name in [
            format!("_{stem}{suffix}.{ext}"),
            format!("{stem}{suffix}.{ext}"),
        ] {
            let cand = dir.join(&name);
            if cand.is_file() {
                found.push(cand);
            }
        }
    }
    Tier::from(found)
}
