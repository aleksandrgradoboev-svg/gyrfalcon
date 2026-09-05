//! Прогон `check_bsl` по настоящей выгрузке: что он находит на ПРАВИЛЬНОМ
//! коде конфигурации — то есть какова доля ложных тревог.
//!
//! Служебный пример, не часть продукта. Отвечает на вопрос «инструмент
//! проверен или только собран»: юнит-тесты гоняют десяток строк, а
//! конфигурация — тысячи модулей, написанных не мной.
use gyrfalcon_mcp::check_bsl::check_bsl;
use gyrfalcon_parser::scan;
use serde_json::json;
use std::collections::HashMap;

fn main() {
    let db = std::env::args().nth(1).expect("путь к индексу");
    let срц = std::env::args().nth(2).expect("путь к выгрузке");
    let лимит: usize = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();

    let paths = scan::collect_modules(std::path::Path::new(&срц));
    let t0 = std::time::Instant::now();
    let (mut модулей, mut проверено, mut пропущено, mut замечаний) = (0usize, 0u64, 0u64, 0usize);
    let mut по_видам: HashMap<String, usize> = HashMap::new();
    let mut без_оговорки = 0usize;
    let mut примеры: Vec<String> = Vec::new();

    for p in paths.iter().take(лимит) {
        let Ok(raw) = std::fs::read(p) else { continue };
        let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
        let s = String::from_utf8_lossy(raw).to_string();
        let Ok(v) = check_bsl(&conn, &json!({"source": s, "limit": 200})) else {
            continue;
        };
        модулей += 1;
        проверено += v["checked_calls"].as_u64().unwrap_or(0);
        пропущено += v["skipped_calls"].as_u64().unwrap_or(0);
        замечаний += v["total"].as_u64().unwrap_or(0) as usize;
        // Оговорка обязана быть в КАЖДОМ ответе — она и есть защита от того,
        // что зелёный вердикт прочитают как «код верный».
        if !v["note"]
            .as_str()
            .map(|s| s.contains("Пропущено"))
            .unwrap_or(false)
        {
            без_оговорки += 1;
        }
        for i in v["issues"].as_array().unwrap() {
            *по_видам
                .entry(i["kind"].as_str().unwrap_or("?").to_string())
                .or_default() += 1;
            if примеры.len() < 12 {
                примеры.push(format!(
                    "{} | {} | {}",
                    p.file_name().unwrap().to_string_lossy(),
                    i["kind"].as_str().unwrap_or("?"),
                    i["message"].as_str().unwrap_or("?")
                ));
            }
        }
    }
    let сек = t0.elapsed().as_secs_f64();
    println!("модулей:            {модулей}");
    println!("вызовов проверено:  {проверено}");
    println!("вызовов пропущено:  {пропущено}");
    println!(
        "ЗАМЕЧАНИЙ:          {замечаний} ({:.2}% от проверенных вызовов)",
        100.0 * замечаний as f64 / проверено.max(1) as f64
    );
    println!("по видам:           {по_видам:?}");
    println!("ответ без оговорки: {без_оговорки}   <- обязан быть 0");
    println!(
        "время:              {сек:.1} с ({:.0} модулей/с)",
        модулей as f64 / сек.max(0.001)
    );
    println!("\n=== примеры (на правильном коде это ЛОЖНЫЕ тревоги) ===");
    for s in &примеры {
        println!("  {s}");
    }
}
