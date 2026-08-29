//! Сверка числа методов по каждому модулю: где мы расходимся с индексом прежнего инструмента.
//!
//! Печатает CSV `rel_path;наше` в stdout. Сопоставление с индексом делает
//! отдельный SQL-запрос — крейта для SQLite здесь пока нет и заводить его
//! ради одной сверки незачем.

use gyrfalcon_parser::{bsl, scan};

fn main() {
    let root_arg = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("укажите путь к каталогу src выгрузки первым аргументом");
        std::process::exit(2);
    });
    let root = std::path::Path::new(&root_arg);
    let paths = scan::collect_modules(root);

    for p in &paths {
        let Ok(raw) = std::fs::read(p) else { continue };
        let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
        let src = String::from_utf8_lossy(raw);

        let n = bsl::parse_module(&src).map(|m| m.len()).unwrap_or(0);

        let rel = p
            .strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        println!("{rel};{n}");
    }
}
