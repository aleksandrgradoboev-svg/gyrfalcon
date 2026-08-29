use std::collections::{HashMap, HashSet};
fn main() {
    // Аргументы: <словарь.db> [корень корпусов]. Корень — каталог, в котором лежат
    // выгрузки конфигураций (<корень>/<имя>/src). Пути машины в коде не хранятся:
    // берётся аргумент, иначе GYRFALCON_CORPUS_ROOT, иначе честный отказ.
    let dict_db = std::env::args().nth(1).unwrap();
    let корень = std::env::args()
        .nth(2)
        .or_else(|| std::env::var("GYRFALCON_CORPUS_ROOT").ok())
        .expect("укажите корень корпусов: аргументом или в GYRFALCON_CORPUS_ROOT");
    let dc = rusqlite::Connection::open(&dict_db).unwrap();
    let словарь: HashSet<String> = dc
        .prepare("SELECT token FROM semantic_dictionary")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .filter_map(|x| x.ok())
        .collect();
    println!("словарь БП: {} токенов\n", словарь.len());
    println!(
        "{:<10} {:>9} {:>9} {:>10} {:>10}",
        "корпус", "имён", "токенов", "новых ток.", "имён 100%"
    );
    for имя in ["zup", "ut", "do", "toir"] {
        let путь = format!("{корень}/{имя}/src");
        let paths = gyrfalcon_parser::scan::collect_modules(std::path::Path::new(&путь));
        if paths.is_empty() {
            println!("{:<10} нет корпуса", имя);
            continue;
        }
        let mut имена: HashSet<String> = HashSet::new();
        for p in paths.iter() {
            if let Ok(raw) = std::fs::read(p) {
                let s =
                    String::from_utf8_lossy(raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw));
                if let Ok(info) = gyrfalcon_parser::module::parse(&s) {
                    for m in info.methods {
                        имена.insert(m.name);
                    }
                }
            }
        }
        let mut токены: HashMap<String, u64> = HashMap::new();
        let (mut полных, mut всего) = (0u64, 0u64);
        for n in &имена {
            let t = gyrfalcon_parser::tokens::tokenize(n);
            if t.is_empty() {
                continue;
            }
            всего += 1;
            if t.iter().all(|x| словарь.contains(x)) {
                полных += 1
            }
            for x in t {
                *токены.entry(x).or_default() += 1
            }
        }
        let новых = токены.keys().filter(|t| !словарь.contains(*t)).count();
        println!(
            "{:<10} {:>9} {:>9} {:>9} ({:>2}%) {:>8.1}%",
            имя,
            всего,
            токены.len(),
            новых,
            100 * новых / токены.len().max(1),
            100.0 * полных as f64 / всего.max(1) as f64
        );
    }
}
