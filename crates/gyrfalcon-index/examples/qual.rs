//! Замер: что даёт словарь семантическому поиску.
//!
//! Один и тот же запрос идёт дважды — по словарю (векторы модели) и по
//! sparse-пути (random indexing от строки, режим «слова в словаре нет»).
//! Оценивает человек: печатается топ-5 обоих режимов рядом.
fn main() {
    let db = std::env::args().nth(1).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let запросы = [
        "себестоимость",
        "покупатель",
        "долг контрагента",
        "остатки на складе",
        "проведение документа",
        "печатная форма счёта",
    ];
    for q in запросы {
        println!("\n=== {q} ===");
        let h = gyrfalcon_index::semantic::search(&conn, q, None, 5).unwrap();
        for (i, x) in h.iter().enumerate() {
            println!("  {}. {:.3} {}", i + 1, x.raw, x.name);
        }
    }
}
