//! Сборка vendored-грамматик tree-sitter.
//!
//! Компилируется по одному C-файлу на грамматику — внешнего сканера у них нет,
//! поэтому обходимся без ручных зависимостей.

use std::path::PathBuf;

fn main() {
    let vendor = PathBuf::from("vendor/tree-sitter-bsl");

    for grammar in ["bsl", "sdbl"] {
        let dir = vendor.join(grammar).join("src");
        let parser = dir.join("parser.c");

        assert!(
            parser.exists(),
            "нет {}: каталог vendor не на месте или испорчен",
            parser.display()
        );

        cc::Build::new()
            .include(&dir)
            .file(&parser)
            // Сгенерированный tree-sitter код шумит предупреждениями,
            // править его нельзя — он машинный.
            .warnings(false)
            .compile(&format!("tree-sitter-{grammar}"));

        println!("cargo:rerun-if-changed={}", parser.display());
    }

    println!("cargo:rerun-if-changed=build.rs");
}
