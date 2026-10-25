//! Default namespaces of scheme urls: `@use "pkg:lib"` binds `lib`, as dart-sass does.

use sasso::{Allocator, Compiler, MemoryImporter, Options};

fn compile(files: &MemoryImporter, entry: &str) -> String {
    let arena = Allocator::default();
    let options = Options::default().with_importer(files);
    Compiler::new(&arena, entry, &options)
        .unwrap()
        .compile()
        .unwrap_or_else(|err| panic!("{}", err.message))
}

#[test]
fn a_scheme_url_binds_its_last_path_segment() {
    let files = MemoryImporter::new()
        .with_file("pkg:lib/_index.scss", "$x: 1px;")
        .with_file("main.scss", "@use \"pkg:lib\";\n.a { b: lib.$x; }\n");
    let css = compile(&files, "main.scss");
    assert!(css.contains("b: 1px"), "{css}");
}
