//! Разбор файлов с ошибками: что именно не разобралось и почему.
//!
//! Служебный пример для вехи 1. Отвечает на вопрос «5 файлов с ошибками —
//! это дефект грамматики или грязь в исходниках», без которого счётчик ошибок
//! остаётся числом без смысла.

use gyrfalcon_parser::{bsl, scan};

fn main() {
    let root_arg = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("укажите путь к каталогу src выгрузки первым аргументом");
        std::process::exit(2);
    });
    let root = std::path::Path::new(&root_arg);
    let paths = scan::collect_modules(root);
    println!("модулей: {}\n", paths.len());

    let mut bad = 0;
    for p in &paths {
        let Ok(raw) = std::fs::read(p) else { continue };
        let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
        let src = String::from_utf8_lossy(raw);

        let Ok(errs) = bsl::check_syntax(&src) else {
            continue;
        };
        if errs.is_empty() {
            continue;
        }
        bad += 1;

        println!("--- {}", p.strip_prefix(root).unwrap_or(p).display());
        for e in errs.iter().take(3) {
            println!("    строка {} кол {}: {}", e.line, e.column, e.message);

            // Показать саму строку: без неё непонятно, на чём споткнулись.
            if let Some(line) = src.lines().nth(e.line as usize - 1) {
                let shown: String = line.chars().take(100).collect();
                println!("      | {}", shown.trim_end());

                // Подозрительные символы — обычная причина на реальных выгрузках.
                let odd: Vec<String> = line
                    .chars()
                    .filter(|c| !c.is_ascii() && !('А'..='я').contains(c) && *c != 'ё' && *c != 'Ё')
                    .map(|c| format!("U+{:04X}", c as u32))
                    .collect();
                if !odd.is_empty() {
                    println!("      небуквенные не-ASCII: {}", odd.join(" "));
                }
            }
        }
        if errs.len() > 3 {
            println!("    ... всего ошибок: {}", errs.len());
        }
    }

    println!("\nфайлов с ошибками: {bad}");
}
