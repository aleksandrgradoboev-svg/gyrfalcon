fn main() {
    let db = std::env::args().nth(1).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let d = gyrfalcon_index::semantic::Dictionary::load(&conn).unwrap();
    println!("словарь: {} токенов\n", d.len());
    for q in [
        "где считается себестоимость",
        "проведение заказа покупателя",
        "остатки товаров на складе",
        "права доступа пользователя",
        "обмен данными с сайтом",
    ] {
        println!("═══ «{q}»");
        // ЛЕКСИКА (FTS5, BM25) — отдельным проходом, Р-016
        let t = std::time::Instant::now();
        let mut st = conn
            .prepare(
                "SELECT m.name, bm25(methods_fts) FROM methods_fts f JOIN methods m ON m.id=f.rowid
             WHERE methods_fts MATCH ?1 ORDER BY bm25(methods_fts) LIMIT 3",
            )
            .unwrap();
        let словом = q.split_whitespace().last().unwrap();
        // Ошибку FTS НЕ проглатываем: молчащий поиск неотличим от
        // «ничего не найдено», и первая же редакция на этом и попалась.
        let лекс: Vec<(String, f64)> =
            match st.query_map([словом], |r| Ok((r.get(0)?, r.get(1)?))) {
                Ok(rows) => rows.map(|x| x.expect("строка FTS")).collect(),
                Err(e) => {
                    println!("     ОШИБКА FTS: {e}");
                    Vec::new()
                }
            };
        println!(
            "  ЛЕКСИКА (по слову «{словом}», {} мс):",
            t.elapsed().as_millis()
        );
        if лекс.is_empty() {
            println!("     — ничего");
        }
        for (n, s) in &лекс {
            println!("     {s:.2}  {n}");
        }
        // СЕМАНТИКА — свой проход, свои оценки
        let t = std::time::Instant::now();
        let hits = gyrfalcon_index::semantic::search(&conn, q, Some("method"), 3).unwrap();
        println!("  СЕМАНТИКА ({} мс):", t.elapsed().as_millis());
        for h in &hits {
            println!("     {:.3}  {}", h.raw, h.name);
        }
        println!();
    }
}
