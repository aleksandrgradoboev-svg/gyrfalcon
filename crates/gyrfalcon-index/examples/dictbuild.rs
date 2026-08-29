use rayon::prelude::*;
use std::collections::HashMap;
fn main() {
    // Аргументы: <выход.db> [корень корпусов]. См. crossconf.rs — путь машины не хардкодится.
    let out = std::env::args().nth(1).unwrap();
    let корень = std::env::args()
        .nth(2)
        .or_else(|| std::env::var("GYRFALCON_CORPUS_ROOT").ok())
        .expect("укажите корень корпусов: аргументом или в GYRFALCON_CORPUS_ROOT");
    let mut freq: HashMap<String, u64> = HashMap::new();
    for конф in ["bp", "zup", "ut", "do", "toir", "mdm", "sppr-test"] {
        let путь = format!("{корень}/{конф}/src");
        let paths = gyrfalcon_parser::scan::collect_modules(std::path::Path::new(&путь));
        if paths.is_empty() {
            eprintln!("{конф}: нет корпуса");
            continue;
        }
        let части: Vec<HashMap<String, u64>> = paths
            .par_chunks(500)
            .map(|ch| {
                let mut f: HashMap<String, u64> = HashMap::new();
                for p in ch {
                    if let Ok(raw) = std::fs::read(p) {
                        let s = String::from_utf8_lossy(
                            raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw),
                        );
                        if let Ok(info) = gyrfalcon_parser::module::parse(&s) {
                            for m in info.methods {
                                for t in gyrfalcon_parser::tokens::tokenize(&m.name) {
                                    *f.entry(t).or_default() += 1;
                                }
                            }
                        }
                    }
                }
                f
            })
            .collect();
        for f in части {
            for (k, v) in f {
                *freq.entry(k).or_default() += v;
            }
        }
        eprintln!("{конф}: словарь вырос до {}", freq.len());
    }
    let conn = rusqlite::Connection::open(&out).unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS semantic_tokens (token TEXT PRIMARY KEY, df INTEGER NOT NULL, idf REAL NOT NULL, from_dictionary INTEGER NOT NULL DEFAULT 0);
                        CREATE TABLE IF NOT EXISTS semantic_dictionary (token TEXT PRIMARY KEY, vector BLOB NOT NULL, source TEXT NOT NULL DEFAULT 'offline');").unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    for (t, f) in &freq {
        tx.execute("INSERT OR REPLACE INTO semantic_tokens(token,df,idf,from_dictionary) VALUES (?1,?2,?3,0)",
            rusqlite::params![t, *f as i64, 1.0f64]).unwrap();
    }
    tx.commit().unwrap();
    println!("ИТОГО {} токенов -> {}", freq.len(), out);
}
