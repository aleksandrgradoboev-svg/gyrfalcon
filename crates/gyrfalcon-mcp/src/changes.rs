//! Карта влияния правки: git diff → задетые методы → радиус поражения.
//!
//! # Зачем инструмент
//!
//! Двенадцатая ось приёмки и единственный инструмент образца, которого у нас
//! не было. Закрывает вопрос ревьюера «что сломает этот MR» — тот самый, ради
//! которого он сейчас ходит руками: открывает diff, выписывает имена
//! изменённых процедур, ищет каждую поиском по тексту, складывает в голове.
//!
//! Индекс отвечает на это за один вызов, потому что граф вызовов уже собран
//! (95,3% разрешения на ЗУП) и в нём есть расстояние в хопах.
//!
//! # Что взято у образца, и что нет
//!
//! Форма перенесена с `detect_changes` из codebase-memory-mcp, прочитанного
//! по исходникам (`src/mcp/mcp.c`), а не по README — README на трёх пунктах
//! из трёх описывает инструмент неточно:
//!
//! 1. **Три источника изменений, а не один.** README говорит «uncommitted
//!    changes», код берёт объединение: `diff <base>...HEAD` (закоммиченное
//!    против базы), `diff` (незакоммиченное) и `status --porcelain`
//!    (неотслеживаемые и новые). Третий источник добавлен у них по багу #520:
//!    `git diff` не видит untracked, и новый файл не появлялся в выдаче
//!    вовсе. Грабли пройдены за нас — берём все три сразу.
//! 2. **База по умолчанию — ветка, а не рабочее дерево.** `base_branch`
//!    с умолчанием `main`; `since` (`HEAD~10`, `v0.5.0`) имеет приоритет
//!    и ложится в ту же трёхточечную семантику `<base>...HEAD`.
//! 3. **Радиус по умолчанию — входящий.** README говорит «blast radius»
//!    без стороны, код ставит `inbound`: транзитивные ВЫЗЫВАЮЩИЕ изменённого.
//!    Это и есть область поражения — те, кого правка может задеть.
//!
//! Классификация риска у образца лежит не здесь, а в `trace_path`, и считает
//! чистое расстояние: hop 1 → CRITICAL, 2 → HIGH, 3 → MEDIUM, дальше LOW.
//! Берём как есть. Соблазн подмешать сюда ширину радиуса, `confidence` рёбер
//! и экспортность был и отвергнут: составная шкала выглядит точным числом,
//! не будучи им, а расстояние — измеримый факт графа.
//!
//! # Чего у образца нет, а у нас есть
//!
//! `has_cross_service` — их флаг «правка выходит за границу процесса»
//! (`HTTP_CALLS`/`ASYNC_CALLS`). Прямого аналога в 1С нет, но есть тот же
//! по смыслу разрыв между тем, что видно в diff, и тем, что исполнится:
//! **перехват расширения**. Правка типового метода, у которого стоит
//! `&Вместо`, до боевой базы может не доехать вовсе. Отсюда `has_overrides`.
//!
//! # Почему git запускается напрямую, а не через оболочку
//!
//! Образец собирает строку команды и отдаёт `sh -c` / `cmd.exe`, из-за чего
//! вынужден отдельно проверять `base_branch` на метасимволы и на ведущий
//! дефис (иначе `--output=<path>` из имени ветки пишет файл куда угодно).
//! Мы передаём аргументы массивом в `Command` — оболочки в цепочке нет,
//! и весь этот класс дыр отпадает по построению, а не по бдительности
//! проверки. Ведущий дефис всё же отсекаем: это не безопасность, а внятный
//! отказ вместо `unknown option` от git.

use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

/// Порог риска по расстоянию — как у образца (`cbm_hop_to_risk`).
fn риск(hop: i64) -> &'static str {
    match hop {
        1 => "CRITICAL",
        2 => "HIGH",
        3 => "MEDIUM",
        _ => "LOW",
    }
}

/// Корень исходников из индекса. Тот же приём, что в доборе (`grep.rs`):
/// путь канонизируется, потому что в индекс он мог попасть в формате 8.3
/// или через символическую ссылку, и `is_dir()` на таком отвечает `false`.
fn корень_исходников(conn: &Connection) -> Result<PathBuf, String> {
    let сырой: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'source_path'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| {
            "в индексе нет source_path — корень исходников неизвестен, \
             карта влияния строится по git в рабочей копии"
                .to_string()
        })?;
    let путь = PathBuf::from(&сырой);
    let путь = std::fs::canonicalize(&путь).unwrap_or(путь);
    if !путь.is_dir() {
        // Отказ, а не пустая выдача: «изменений нет» и «каталог не найден» —
        // разные факты, и путать их нельзя. Пустой ответ прочли бы как
        // «правок нет», то есть как разрешение не смотреть.
        return Err(format!(
            "каталог исходников не найден: {}. Индекс собран на другой машине \
             или выгрузка перемещена — git запускается в рабочей копии, а не в индексе",
            путь.display()
        ));
    }
    Ok(путь)
}

/// Запустить git и вернуть stdout. Аргументы идут массивом — оболочки нет.
///
/// `core.quotepath=false` обязателен и поймана его нужда живой пробой:
/// по умолчанию git отдаёт неASCII-пути октальными escape-последовательностями
/// в кавычках (`"cfe/\320\237\320\276..."`). В 1С кириллица в именах файлов —
/// норма, а не редкость, поэтому без этого ключа выдача нечитаема, и, что
/// хуже, ни один такой путь не совпадёт с `modules.rel_path`: инструмент
/// молча возвращал бы «изменённые файлы есть, методов не нашлось».
fn git(корень: &PathBuf, аргументы: &[&str]) -> Result<String, String> {
    let вывод = Command::new("git")
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("-C")
        .arg(корень)
        .args(аргументы)
        .output()
        .map_err(|e| format!("git не запустился: {e}. Проверьте, что git установлен и в PATH"))?;
    // stderr намеренно не превращаем в ошибку: `git diff` против несуществующей
    // ветки ругается, но соседние источники изменений при этом рабочие.
    // Пустой stdout здесь — законный результат «по этому источнику ничего».
    Ok(String::from_utf8_lossy(&вывод.stdout).into_owned())
}

/// Изменённые файлы из трёх источников — объединением, как у образца.
fn изменённые_файлы(корень: &PathBuf, база: &str) -> Result<Vec<String>, String> {
    let mut набор: BTreeMap<String, ()> = BTreeMap::new();

    // 1. закоммиченное против базы; трёхточие = от точки расхождения,
    //    иначе в выдачу попадут чужие правки, приехавшие в base после ветвления
    let диапазон = format!("{база}...HEAD");
    for строка in git(корень, &["diff", "--name-only", &диапазон])?.lines() {
        набор.insert(строка.trim().to_string(), ());
    }
    // 2. незакоммиченное в рабочем дереве
    for строка in git(корень, &["diff", "--name-only"])?.lines() {
        набор.insert(строка.trim().to_string(), ());
    }
    // 3. неотслеживаемое и добавленное в индекс git — их не видит `diff`
    //    (баг #520 образца: новый файл не появлялся до переиндексации).
    //    Формат строки: два символа кода + пробел + путь («?? a/b.bsl»).
    for строка in git(
        корень,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=normal",
        ],
    )?
    .lines()
    {
        if строка.len() > 3 {
            набор.insert(строка[3..].trim().to_string(), ());
        }
    }

    набор.remove("");
    Ok(набор.into_keys().collect())
}

pub fn detect_changes(conn: &Connection, args: &Value) -> Result<Value, String> {
    // `since` перекрывает `base_branch` — приоритет тот же, что у образца.
    let база = args
        .get("since")
        .and_then(Value::as_str)
        .or_else(|| args.get("base_branch").and_then(Value::as_str))
        .unwrap_or("main")
        .to_string();
    if база.starts_with('-') {
        return Err(format!(
            "ссылка «{база}» начинается с дефиса — git прочтёт её как опцию, а не как ref"
        ));
    }
    let направление = args
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or("inbound")
        .to_string();
    // Неизвестное направление НЕ исправляем молча — приём образца, и довод
    // его же: вызывающий иначе неверно прочтёт семантику результата.
    if !matches!(направление.as_str(), "inbound" | "outbound" | "both") {
        return Err(format!(
            "неизвестное направление «{направление}» — возможны \
             «inbound» (радиус поражения: транзитивные вызывающие), \
             «outbound» (от чего зависит правка) или «both»"
        ));
    }
    let глубина = args
        .get("depth")
        .and_then(Value::as_i64)
        .unwrap_or(2)
        .clamp(1, 5);
    let предел = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(200)
        .clamp(1, 2000);
    let только_файлы = args.get("scope").and_then(Value::as_str) == Some("files");

    let корень = корень_исходников(conn)?;
    let файлы = изменённые_файлы(&корень, &база)?;

    if файлы.is_empty() {
        return Ok(json!({
            "base": база,
            "changed_files": 0,
            "note": "git не показал изменений против этой базы — \
                     ни закоммиченных, ни в рабочем дереве, ни новых файлов"
        }));
    }

    let mut ответ = json!({
        "base": база,
        "direction": направление,
        "depth": глубина,
        "changed_files": файлы.len(),
        "files": файлы,
    });
    if только_файлы {
        return Ok(ответ);
    }

    // Файлы diff — пути от корня репозитория, а в индексе `modules.rel_path`
    // отсчитывается от корня ВЫГРУЗКИ. Он лежит глубже (у ДО это `src/`,
    // а расширения вовсе в `../ext/`), поэтому сверяем по хвосту пути,
    // а не по равенству: иначе совпадений не будет ни одного.
    let mut семена: Vec<(String, String)> = Vec::new(); // (метод, модуль)
    for файл in &файлы {
        let хвост = файл.trim_start_matches("./");
        let mut st = conn
            .prepare(
                "SELECT m.name, mo.rel_path FROM methods m
                 JOIN modules mo ON mo.id = m.module_id
                 WHERE ?1 LIKE '%' || mo.rel_path
                 ORDER BY m.line",
            )
            .map_err(|e| e.to_string())?;
        let строки = st
            .query_map([&хвост], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        for с in строки.flatten() {
            семена.push(с);
        }
    }

    ответ["changed_methods"] = json!(семена.len());
    if семена.is_empty() {
        // Файлы изменены, а методов не нашлось — законный и частый случай в 1С:
        // правка XML метаданных, формы или роли кода не касается вовсе.
        ответ["note"] = json!(
            "изменённые файлы не содержат проиндексированных методов — \
             правка метаданных, формы или роли, а не кода"
        );
        return Ok(ответ);
    }

    // Радиус: транзитивные вызывающие с расстоянием. Тот же рекурсивный CTE,
    // что у `callers`, но семян много — стартовое множество задаётся списком,
    // и обход идёт по всем сразу, а не N вызовами.
    let имена: Vec<String> = семена.iter().map(|(n, _)| n.clone()).collect();
    let плейсхолдеры: Vec<String> = (1..=имена.len()).map(|i| format!("?{i}")).collect();
    let список = плейсхолдеры.join(",");
    let п_глубина = имена.len() + 1;
    let п_предел = имена.len() + 2;

    let mut параметры: Vec<&dyn rusqlite::ToSql> = имена
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let mut параметры_сводки = параметры.clone();
    параметры_сводки.push(&глубина);
    параметры.push(&глубина);
    параметры.push(&предел);

    if направление == "inbound" || направление == "both" {
        let sql = format!(
            "WITH RECURSIVE радиус(name, hop) AS (
                 SELECT name, 0 FROM methods WHERE name IN ({список})
                 UNION
                 SELECT m.name, р.hop + 1
                 FROM радиус р
                 JOIN calls c ON c.callee_name = р.name COLLATE NOCASE
                 JOIN methods m ON m.id = c.caller_id
                 WHERE р.hop < ?{п_глубина}
             )
             SELECT р.name, MIN(р.hop) AS hop, mo.rel_path
             FROM радиус р
             JOIN methods m ON m.name = р.name COLLATE NOCASE
             JOIN modules mo ON mo.id = m.module_id
             WHERE р.hop > 0
             GROUP BY р.name, mo.rel_path
             ORDER BY hop, р.name LIMIT ?{п_предел}"
        );
        let sql_сводка = format!(
            "WITH RECURSIVE радиус(name, hop) AS (
                 SELECT name, 0 FROM methods WHERE name IN ({список})
                 UNION
                 SELECT m.name, р.hop + 1
                 FROM радиус р
                 JOIN calls c ON c.callee_name = р.name COLLATE NOCASE
                 JOIN methods m ON m.id = c.caller_id
                 WHERE р.hop < ?{п_глубина}
             )
             SELECT hop, COUNT(*) FROM (
                 SELECT р.name, MIN(р.hop) AS hop FROM радиус р
                 WHERE р.hop > 0 GROUP BY р.name
             ) GROUP BY hop"
        );
        ответ["blast_radius"] =
            радиус_с_риском(conn, &sql, &sql_сводка, &параметры, &параметры_сводки)?;
    }
    if направление == "outbound" || направление == "both" {
        let sql = format!(
            "WITH RECURSIVE зависимости(name, hop) AS (
                 SELECT name, 0 FROM methods WHERE name IN ({список})
                 UNION
                 SELECT c.callee_name, з.hop + 1
                 FROM зависимости з
                 JOIN methods m ON m.name = з.name COLLATE NOCASE
                 JOIN calls c ON c.caller_id = m.id
                 WHERE з.hop < ?{п_глубина}
             )
             SELECT з.name, MIN(з.hop) AS hop, '' AS rel_path
             FROM зависимости з
             WHERE з.hop > 0
             GROUP BY з.name
             ORDER BY hop, з.name LIMIT ?{п_предел}"
        );
        let sql_сводка = format!(
            "WITH RECURSIVE зависимости(name, hop) AS (
                 SELECT name, 0 FROM methods WHERE name IN ({список})
                 UNION
                 SELECT c.callee_name, з.hop + 1
                 FROM зависимости з
                 JOIN methods m ON m.name = з.name COLLATE NOCASE
                 JOIN calls c ON c.caller_id = m.id
                 WHERE з.hop < ?{п_глубина}
             )
             SELECT hop, COUNT(*) FROM (
                 SELECT з.name, MIN(з.hop) AS hop FROM зависимости з
                 WHERE з.hop > 0 GROUP BY з.name
             ) GROUP BY hop"
        );
        ответ["dependencies"] =
            радиус_с_риском(conn, &sql, &sql_сводка, &параметры, &параметры_сводки)?;
    }

    // Перехваты расширений — наш аналог `has_cross_service` образца.
    // Считаем по именам семян: если изменённый метод кем-то перехвачен,
    // в боевой базе исполнится не то, что видно в diff.
    if let Ok(мера) = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM extension_overrides WHERE method_name IN ({список})"
        ),
        rusqlite::params_from_iter(имена.iter()),
        |r| r.get::<_, i64>(0),
    ) {
        ответ["has_overrides"] = json!(мера > 0);
        if мера > 0 {
            ответ["overrides_note"] = json!(
                "изменённые методы перехвачены расширениями — \
                 проверьте overrides: исполнится не то, что в diff"
            );
        }
    }

    Ok(ответ)
}

/// Выборка радиуса с ярлыком риска и сводкой по уровням.
///
/// Сводка `by_risk` считается по ПОЛНОМУ радиусу, а список строк режется
/// по `limit` — и это не небрежность, а разделение ролей: перечень нужен
/// обозримый, а счёт обязан быть верным. Поймано живой пробой: с общей
/// выборкой предел срезал выдачу на первом хопе, и сводка показывала
/// «CRITICAL: 30», хотя дальних вызывающих было больше. Итог выглядел
/// страшнее правды — а сводке верят именно потому, что она короткая.
fn радиус_с_риском(
    conn: &Connection,
    sql: &str,
    sql_сводка: &str,
    параметры: &[&dyn rusqlite::ToSql],
    параметры_сводки: &[&dyn rusqlite::ToSql],
) -> Result<Value, String> {
    // Сводка — по всему радиусу, без предела.
    let mut ст_св = conn.prepare(sql_сводка).map_err(|e| e.to_string())?;
    let строки_св = ст_св
        .query_map(параметры_сводки, |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|e| e.to_string())?;
    let mut сводка: BTreeMap<&str, i64> = BTreeMap::new();
    let mut всего = 0i64;
    for с in строки_св.flatten() {
        let (hop, сколько) = с;
        *сводка.entry(риск(hop)).or_insert(0) += сколько;
        всего += сколько;
    }

    let mut st = conn.prepare(sql).map_err(|e| e.to_string())?;
    let строки = st
        .query_map(параметры, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut набор = Vec::new();
    for с in строки.flatten() {
        let (имя, hop, путь) = с;
        набор.push(json!([имя, hop, риск(hop), путь]));
    }
    let срезано = всего > набор.len() as i64;
    let mut ответ = json!({
        "columns": ["name", "hop", "risk", "module"],
        "total": всего,
        "shown": набор.len(),
        "by_risk": сводка,
        "rows": набор,
    });
    if срезано {
        ответ["note"] = json!(
            "показаны ближайшие по расстоянию; by_risk и total — по всему радиусу, \
             не по показанному. Поднимите limit, чтобы увидеть остальных"
        );
    }
    Ok(ответ)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пороги риска — те же, что у образца (`cbm_hop_to_risk`).
    #[test]
    fn пороги_риска_как_у_образца() {
        assert_eq!(риск(1), "CRITICAL");
        assert_eq!(риск(2), "HIGH");
        assert_eq!(риск(3), "MEDIUM");
        assert_eq!(риск(4), "LOW");
        assert_eq!(риск(99), "LOW");
    }

    fn индекс_в_памяти() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE index_meta (key TEXT, value TEXT);
             CREATE TABLE modules (id INTEGER PRIMARY KEY, rel_path TEXT);
             CREATE TABLE methods (id INTEGER PRIMARY KEY, name TEXT, module_id INTEGER, line INTEGER);
             CREATE TABLE calls (id INTEGER PRIMARY KEY, caller_id INTEGER, callee_name TEXT);
             CREATE TABLE extension_overrides (method_name TEXT);",
        )
        .unwrap();
        conn
    }

    /// Нет корня исходников — ОТКАЗ, а не пустая выдача: «изменений нет»
    /// и «смотреть негде» разные факты.
    #[test]
    fn без_корня_отказ_а_не_пустота() {
        let conn = индекс_в_памяти();
        let e = detect_changes(&conn, &json!({})).unwrap_err();
        assert!(e.contains("source_path"), "неожиданный текст: {e}");
    }

    /// Неизвестное направление не исправляется молча.
    #[test]
    fn неизвестное_направление_учит_а_не_молчит() {
        let conn = индекс_в_памяти();
        conn.execute(
            "INSERT INTO index_meta VALUES ('source_path', ?1)",
            [std::env::temp_dir().to_string_lossy().to_string()],
        )
        .unwrap();
        let e = detect_changes(&conn, &json!({"direction": "вверх"})).unwrap_err();
        assert!(e.contains("inbound"), "отказ не назвал допустимые: {e}");
    }

    /// Сводка считается по ПОЛНОМУ радиусу, а не по обрезанному списком.
    /// Регрессия, пойманная живой пробой 31.08.2026: с общим запросом предел
    /// срезал выдачу на первом хопе, и `by_risk` показывал «CRITICAL: 30»
    /// при 143 задетых методах — итог выглядел страшнее правды.
    #[test]
    fn сводка_по_полному_радиусу_а_не_по_показанному() {
        let conn = индекс_в_памяти();
        conn.execute_batch(
            "INSERT INTO modules VALUES (1, 'CommonModules/М/Ext/Module.bsl');
             INSERT INTO methods VALUES (1, 'Корень', 1, 1);
             INSERT INTO methods VALUES (2, 'А', 1, 2);
             INSERT INTO methods VALUES (3, 'Б', 1, 3);
             INSERT INTO methods VALUES (4, 'В', 1, 4);
             INSERT INTO calls VALUES (1, 2, 'Корень');
             INSERT INTO calls VALUES (2, 3, 'Корень');
             INSERT INTO calls VALUES (3, 4, 'Корень');",
        )
        .unwrap();
        let sql = "SELECT m.name, 1 AS hop, '' FROM methods m WHERE m.name<>'Корень' \
                   ORDER BY m.name LIMIT 1";
        let сводка = "SELECT 1, COUNT(*) FROM methods WHERE name<>'Корень'";
        let r = радиус_с_риском(&conn, sql, сводка, &[], &[]).unwrap();
        assert_eq!(r["total"], 3, "сводка обязана считать весь радиус");
        assert_eq!(r["shown"], 1, "список режется пределом");
        assert_eq!(r["by_risk"]["CRITICAL"], 3);
        assert!(r["note"].is_string(), "срез списка должен быть назван вслух");
    }

    /// Ведущий дефис в ref — внятный отказ, а не `unknown option` от git.
    #[test]
    fn ведущий_дефис_отсекается() {
        let conn = индекс_в_памяти();
        let e = detect_changes(&conn, &json!({"since": "--output=/tmp/x"})).unwrap_err();
        assert!(e.contains("дефис"), "неожиданный текст: {e}");
    }
}
