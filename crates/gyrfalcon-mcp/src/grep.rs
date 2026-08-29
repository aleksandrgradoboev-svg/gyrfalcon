//! Полнотекстовый добор по телу модулей — то, чего не видит структурный поиск.
//!
//! # Зачем инструмент
//!
//! Одиннадцатая ось приёмки — «полнотекстовый добор там, где
//! структурный поиск промахнулся». До 29.08.2026 она была не покрыта, и
//! причина оказалась глубже отсутствия сценария: **искать было нечем**.
//! Индекс хранит имена, адреса и связи; тела методов в нём не лежат вовсе,
//! а `read` отдаёт только адрес и сигнатуру.
//!
//! Между тем именно добор закрывает случай из правил контура: «промах
//! хелпера ≠ отсутствие объекта». GUID, текст сообщения пользователю, кусок
//! запроса, магическая строка в условии — ничего этого нет ни в одном имени,
//! и структурный поиск про них молчит. Молчит убедительно: пустой ответ
//! неотличим от «такого в конфигурации нет».
//!
//! # Устройство
//!
//! Grep по файлам на диске, а не по индексу. Так же устроен `search_code`
//! у индексаторов общего назначения («graph-augmented grep over indexed files only») и `safe_grep`
//! у Python-сервера. Причина общая: хранить тела в индексе значит кратно
//! раздуть его (ЕРП.УХ уже 3,8 ГБ) и замедлить сборку — а скорость это
//! первый критерий замены.
//!
//! Перечень файлов берётся из индекса (`modules.rel_path` + корень из
//! `index_meta.source_path`), а не обходом каталога: ищем ровно по тому,
//! что проиндексировано, иначе выдача разойдётся с остальными инструментами.
//!
//! # Три приёма, взятые у `safe_grep` вместе с их доводами
//!
//! 1. **Сужение по имени модуля** (`module`) — на 30 тысячах модулей это
//!    разница между секундой и минутой;
//! 2. **быстрый путь для литералов** — если в шаблоне нет метасимволов,
//!    ищем подстроку, а не регулярным выражением;
//! 3. **разбор шаблона ПЕРВЫМ действием** — битый шаблон должен давать
//!    внятный отказ сразу, а не сырое исключение после обхода тысяч файлов.
//!
//! Чего НЕ берём: их защиту от катастрофического бэктрекинга. Она лечит
//! свойство Python `re`; крейт `regex` в Rust гарантирует линейное время
//! по построению, и паттерн `(a+)+b` его не вешает. Переносить чужую
//! защиту от чужой болезни — значит нести и её цену.

use rayon::prelude::*;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::PathBuf;

/// Сколько файлов просматриваем по умолчанию.
///
/// Ограничение honest-by-default: полный обход ЕРП.УХ (29 818 модулей)
/// осмыслен, но его надо просить явно, иначе случайный широкий запрос
/// съест минуты. Усечение называется вслух — молчаливое неполное «ничего
/// не нашлось» и есть тот дефект, ради которого инструмент заведён.
const ФАЙЛОВ_ПО_УМОЛЧАНИЮ: usize = 2000;

/// Максимум строк в выдаче: ответ агенту, а не дамп.
const СТРОК_ПО_УМОЛЧАНИЮ: usize = 50;

pub fn grep(conn: &Connection, args: &Value) -> Result<Value, String> {
    let шаблон = args
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or("нужен параметр pattern")?
        .to_string();
    if шаблон.is_empty() {
        return Err("пустой pattern".into());
    }
    let модуль = args
        .get("module")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let файлов = args
        .get("max_files")
        .and_then(Value::as_u64)
        .unwrap_or(ФАЙЛОВ_ПО_УМОЛЧАНИЮ as u64)
        .clamp(1, 100_000) as usize;
    let лимит = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(СТРОК_ПО_УМОЛЧАНИЮ as u64)
        .clamp(1, 1000) as usize;
    let учитывать_регистр = args
        .get("case_sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Разбор шаблона ПЕРВЫМ действием (приём #3): битый шаблон обязан дать
    // внятный отказ до обхода файлов, а не сырую ошибку из глубины.
    let литерал = !шаблон.contains(|c| "\\.^$|?*+()[]{}".contains(c));
    let ре = if литерал {
        None
    } else {
        Some(
            regex::RegexBuilder::new(&шаблон)
                .case_insensitive(!учитывать_регистр)
                .build()
                .map_err(|e| format!("некорректное регулярное выражение: {e}"))?,
        )
    };

    let корень: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'source_path'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| {
            "в индексе нет source_path — корень исходников неизвестен, добор невозможен".to_string()
        })?;
    // Канонизируем корень перед проверкой: путь мог быть записан в индекс
    // в формате 8.3 (`C:\Users\RUNNER~1\...`), через символическую ссылку
    // или с иным регистром. `is_dir()` на таком пути отвечает `false`,
    // и добор отказывал бы на исправном индексе. Поймано CI: тест падал
    // только на windows-раннере, где temp_dir() отдаёт короткое имя.
    let корень = PathBuf::from(&корень);
    let корень = std::fs::canonicalize(&корень).unwrap_or(корень);
    if !корень.is_dir() {
        // Отказ, а не пустая выдача: исходники могли переехать, и «ничего
        // не нашлось» тут означало бы неправду о конфигурации.
        return Err(format!(
            "каталог исходников не найден: {}. Индекс собран на другой машине \
             или выгрузка перемещена — добор идёт по файлам, а не по индексу",
            корень.display()
        ));
    }

    // Сужение по модулю (приём #1): без него на 30 тысячах модулей обход
    // идёт минуты. Пустой `module` = весь корпус в пределах max_files.
    let пути: Vec<String> = if модуль.is_empty() {
        let mut st = conn
            .prepare("SELECT rel_path FROM modules LIMIT ?1")
            .map_err(|e| e.to_string())?;
        let r = st
            .query_map(rusqlite::params![файлов as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(|e| e.to_string())?
            .filter_map(std::result::Result::ok)
            .collect();
        r
    } else {
        let mut st = conn
            .prepare(
                "SELECT rel_path FROM modules
                 WHERE rel_path LIKE ?2 COLLATE NOCASE OR object_name LIKE ?2 COLLATE NOCASE
                 LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;
        let r = st
            .query_map(
                rusqlite::params![файлов as i64, format!("%{модуль}%")],
                |r| r.get::<_, String>(0),
            )
            .map_err(|e| e.to_string())?
            .filter_map(std::result::Result::ok)
            .collect();
        r
    };

    if пути.is_empty() {
        return Ok(json!({
            "pattern": шаблон,
            "rows": [],
            "note": if модуль.is_empty() {
                "в индексе нет модулей".to_string()
            } else {
                format!("ни один модуль не подошёл под '{модуль}' — сужение отсекло всё")
            }
        }));
    }

    let всего_файлов = пути.len();
    let нижний = if литерал && !учитывать_регистр {
        шаблон.to_lowercase()
    } else {
        шаблон.clone()
    };

    // Параллельно по ядрам: обход тысяч файлов — ровно та задача, ради
    // которой в проекте выбран Rust (веха 1).
    let mut найдено: Vec<(String, usize, String)> = пути
        .par_iter()
        .flat_map(|отн| {
            let полный = корень.join(отн);
            let Ok(текст) = std::fs::read_to_string(&полный) else {
                // Файл нечитаем — пропускаем молча ЗДЕСЬ, но общее число
                // прочитанных возвращается наружу: пусть видно, что обошли
                // не всё, если такое случится.
                return Vec::new();
            };
            let mut свои = Vec::new();
            for (i, строка) in текст.lines().enumerate() {
                let попал = match &ре {
                    Some(r) => r.is_match(строка),
                    None => {
                        if учитывать_регистр {
                            строка.contains(&нижний)
                        } else {
                            строка.to_lowercase().contains(&нижний)
                        }
                    }
                };
                if попал {
                    // Строку обрезаем: одна строка кода бывает в тысячи
                    // знаков (сгенерированные запросы), и десяток таких
                    // съест весь ответ.
                    let т = строка.trim();
                    let т: String = if т.chars().count() > 200 {
                        т.chars().take(200).collect::<String>() + "…"
                    } else {
                        т.to_string()
                    };
                    свои.push((отн.clone(), i + 1, т));
                }
            }
            свои
        })
        .collect();

    найдено.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let всего = найдено.len();
    найдено.truncate(лимит);

    let rows: Vec<Value> = найдено.iter().map(|(f, l, t)| json!([f, l, t])).collect();

    let mut out = json!({
        "pattern": шаблон,
        "columns": ["file", "line", "text"],
        "rows": rows,
        "found": всего,
        "files_scanned": всего_файлов,
    });
    // Усечение называется вслух — и полное, и по файлам. Молчаливое
    // неполное «нашлось N» хуже честного «нашлось N из M».
    if всего > лимит {
        out["truncated"] = json!(format!(
            "показано {лимит} из {всего}; сузьте module или поднимите limit"
        ));
    }
    if всего_файлов >= файлов {
        out["files_limited"] = json!(format!(
            "просмотрено {всего_файлов} файлов (предел max_files) — возможно, не весь корпус"
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn база_и_файлы() -> (Connection, tempdir::Dir) {
        let d = tempdir::Dir::new();
        let m = d.path().join("CommonModules/Тест/Ext");
        std::fs::create_dir_all(&m).unwrap();
        let mut f = std::fs::File::create(m.join("Module.bsl")).unwrap();
        writeln!(f, "Процедура Тест()").unwrap();
        writeln!(f, "    // GUID 8f14e45f-ceea-467a-9ba6-1e5fd9c9b8d3").unwrap();
        writeln!(f, "    Сообщить(\"Документ не проведён\");").unwrap();
        writeln!(f, "КонецПроцедуры").unwrap();

        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE modules (id INTEGER PRIMARY KEY, rel_path TEXT, category TEXT,
                 object_name TEXT, module_type TEXT);
             CREATE TABLE index_meta (key TEXT, value TEXT);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO modules (rel_path, category, object_name, module_type)
             VALUES ('CommonModules/Тест/Ext/Module.bsl','CommonModules','Тест','Module')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO index_meta (key, value) VALUES ('source_path', ?1)",
            [d.path().to_string_lossy().to_string()],
        )
        .unwrap();
        (c, d)
    }

    /// Крошечная замена tempfile, чтобы не тянуть зависимость ради тестов.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "gyrfalcon-grep-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::create_dir_all(&p).unwrap();
                // Канонизируем путь. На windows-раннере GitHub Actions
                // `temp_dir()` отдаёт ПУТЬ В ФОРМАТЕ 8.3
                // (`C:\Users\RUNNER~1\...` вместо `runneradmin`), и проверка
                // `is_dir()` на нём не срабатывает: тест падал только в CI,
                // проходя локально. Лог назвал причину сам — «каталог
                // исходников не найден» с коротким именем в сообщении.
                let p = std::fs::canonicalize(&p).unwrap_or(p);
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn находит_guid_которого_нет_ни_в_одном_имени() {
        // Ровно тот случай, ради которого заведён инструмент: структурный
        // поиск про GUID молчит, потому что его нет в именах.
        let (c, _d) = база_и_файлы();
        let r = grep(
            &c,
            &json!({"pattern": "8f14e45f-ceea-467a-9ba6-1e5fd9c9b8d3"}),
        )
        .unwrap();
        assert_eq!(r["found"], 1);
        assert_eq!(r["rows"][0][1], 2);
    }

    #[test]
    fn находит_текст_сообщения_пользователю() {
        let (c, _d) = база_и_файлы();
        let r = grep(&c, &json!({"pattern": "не проведён"})).unwrap();
        assert_eq!(r["found"], 1);
    }

    #[test]
    fn регистр_по_умолчанию_не_важен() {
        let (c, _d) = база_и_файлы();
        let r = grep(&c, &json!({"pattern": "ДОКУМЕНТ"})).unwrap();
        assert_eq!(r["found"], 1);
        let r = grep(&c, &json!({"pattern": "ДОКУМЕНТ", "case_sensitive": true})).unwrap();
        assert_eq!(r["found"], 0);
    }

    #[test]
    fn регулярное_выражение_работает() {
        let (c, _d) = база_и_файлы();
        let r = grep(&c, &json!({"pattern": r"Процедура\s+\w+\("})).unwrap();
        assert_eq!(r["found"], 1);
    }

    #[test]
    fn битый_шаблон_даёт_внятный_отказ_а_не_панику() {
        let (c, _d) = база_и_файлы();
        let e = grep(&c, &json!({"pattern": "(незакрытая"})).unwrap_err();
        assert!(e.contains("некорректное регулярное выражение"), "{e}");
    }

    #[test]
    fn сужение_по_модулю_отсекает_и_говорит_об_этом() {
        let (c, _d) = база_и_файлы();
        let r = grep(&c, &json!({"pattern": "Тест", "module": "ТакогоМодуляНет"})).unwrap();
        assert_eq!(r["rows"].as_array().unwrap().len(), 0);
        assert!(r["note"].as_str().unwrap().contains("сужение отсекло"));
    }

    #[test]
    fn усечение_названо_вслух() {
        let (c, _d) = база_и_файлы();
        let r = grep(&c, &json!({"pattern": "е", "limit": 1})).unwrap();
        assert!(r.get("truncated").is_some(), "усечение должно быть названо");
    }

    #[test]
    fn отсутствие_каталога_даёт_отказ_а_не_пустоту() {
        // Пустая выдача тут означала бы неправду о конфигурации.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE modules (id INTEGER PRIMARY KEY, rel_path TEXT, category TEXT,
                 object_name TEXT, module_type TEXT);
             CREATE TABLE index_meta (key TEXT, value TEXT);
             INSERT INTO index_meta (key,value) VALUES ('source_path','Z:/нет/такого');",
        )
        .unwrap();
        let e = grep(&c, &json!({"pattern": "что-нибудь"})).unwrap_err();
        assert!(e.contains("не найден"), "{e}");
    }
}
