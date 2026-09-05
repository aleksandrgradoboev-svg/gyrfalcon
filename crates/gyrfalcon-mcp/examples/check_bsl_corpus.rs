//! Прогон `check_bsl` по настоящей выгрузке: ловит ли он то, что должен,
//! и не поднимает ли ложных тревог на живом коде.
//!
//! Служебный пример, не часть продукта. Отвечает на вопрос «инструмент
//! проверен или только собран»: юнит-тесты гоняют семь строк, а конфигурация —
//! десятки тысяч модулей, написанных не мной.

use gyrfalcon_mcp::check_bsl::check_bsl;
use gyrfalcon_parser::scan;
use serde_json::json;

fn main() {
    let root_arg = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("укажите путь к каталогу выгрузки первым аргументом");
        std::process::exit(2);
    });
    let root = std::path::Path::new(&root_arg);
    let paths = scan::collect_modules(root);
    println!("модулей: {}", paths.len());

    let t0 = std::time::Instant::now();
    let mut всего = 0usize;
    let mut с_ошибками = 0usize;
    let mut ошибок_всего = 0usize;
    let mut без_фрагмента = 0usize;
    let mut без_оговорки = 0usize;
    let mut странных = 0usize;
    let mut отказов = 0usize;
    let mut примеры: Vec<String> = Vec::new();

    for p in &paths {
        let Ok(raw) = std::fs::read(p) else { continue };
        let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
        let src = String::from_utf8_lossy(raw).to_string();
        всего += 1;

        let Ok(v) = check_bsl(&json!({ "source": src, "limit": 5 })) else {
            отказов += 1;
            continue;
        };

        // Оговорка обязана быть в КАЖДОМ ответе — она и есть защита от того,
        // что зелёный вердикт прочитают как «код верный».
        if !v["note"]
            .as_str()
            .map(|s| s.contains("ТОЛЬКО структура"))
            .unwrap_or(false)
        {
            без_оговорки += 1;
        }

        let n = v["total"].as_u64().unwrap_or(0) as usize;
        if n == 0 {
            continue;
        }
        с_ошибками += 1;
        ошибок_всего += n;

        for e in v["errors"].as_array().unwrap() {
            // Координата обязана указывать на существующую строку, иначе
            // «строка 500» в файле из 40 строк — это ложь, а не диагностика.
            let line = e["line"].as_u64().unwrap_or(0) as usize;
            if line == 0 || line > src.lines().count() {
                странных += 1;
            }
            if e.get("fragment").is_none() {
                без_фрагмента += 1;
            }
        }

        if примеры.len() < 8 {
            let e = &v["errors"][0];
            примеры.push(format!(
                "{}\n      строка {} кол {}: {} | {}",
                p.strip_prefix(root).unwrap_or(p).display(),
                e["line"],
                e["column"],
                e["message"].as_str().unwrap_or("?"),
                e["fragment"].as_str().unwrap_or("<нет фрагмента>").trim()
            ));
        }
    }

    let сек = t0.elapsed().as_secs_f64();
    println!("\n=== ИТОГ ===");
    println!("проверено модулей:      {всего}");
    println!("с ошибками:             {с_ошибками} ({:.3}%)", 100.0 * с_ошибками as f64 / всего.max(1) as f64);
    println!("ошибок всего:           {ошибок_всего}");
    println!("отказов инструмента:    {отказов}");
    println!("координата вне файла:   {странных}   <- обязан быть 0");
    println!("ошибка без фрагмента:   {без_фрагмента}");
    println!("ответ без оговорки:     {без_оговорки}   <- обязан быть 0");
    println!("время:                  {сек:.1} с ({:.0} модулей/с)", всего as f64 / сек.max(0.001));

    if !примеры.is_empty() {
        println!("\n=== примеры находок ===");
        for s in &примеры {
            println!("  {s}");
        }
    }
}
