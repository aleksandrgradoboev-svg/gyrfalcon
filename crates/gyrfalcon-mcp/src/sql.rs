//! Read-only SQL к индексу — предохранитель решения Р-101.
//!
//! Набор инструментов — это множество вопросов, которые удалось предвидеть.
//! SQL нужен для вопросов за его пределами. Плата названа в Р-101 прямо:
//! ошибка в запросе даёт молчаливо неверный ответ, а не отказ. Отсюда всё,
//! что ниже.
//!
//! # Три рубежа, а не один
//!
//! 1. **соединение открыто только на чтение** (`SQLITE_OPEN_READ_ONLY`) —
//!    единственный рубеж, который держит не текст запроса, а сама SQLite;
//! 2. **разбор запроса**: ровно один оператор, начинается с `SELECT` или
//!    `WITH`, запрещены `ATTACH`/`PRAGMA` и прочее;
//! 3. **лимит строк** — чтобы ответ не вынес контекст агента.
//!
//! Первый рубеж главный. Проверка текста запроса — вспомогательная: она даёт
//! понятный отказ вместо ошибки SQLite, но полагаться на неё одну нельзя,
//! потому что обойти разбор текста всегда проще, чем права соединения.

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::Path;

/// Потолок строк в ответе, если вызывающий не задал свой.
pub const DEFAULT_LIMIT: usize = 200;
/// Жёсткий потолок: больше не отдаём даже по явной просьбе.
pub const MAX_LIMIT: usize = 2000;

#[derive(Debug)]
pub enum SqlError {
    /// Запрос отвергнут разбором — с причиной, понятной агенту.
    Rejected(String),
    /// SQLite ответила ошибкой (синтаксис, нет таблицы, …).
    Sqlite(String),
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlError::Rejected(s) => write!(f, "запрос отклонён: {s}"),
            SqlError::Sqlite(s) => write!(f, "SQLite: {s}"),
        }
    }
}

/// Открыть индекс строго на чтение.
///
/// Главный рубеж защиты: даже если разбор текста что-то пропустит, запись
/// невозможна на уровне соединения.
///
/// С ожиданием занятости: индекс может писать `gyrfalcon update`, запущенный
/// рядом отдельным процессом, — замок проекта внутри сервера его не видит.
/// Без ожидания чтение на его коммите отказало бы `database is locked`
/// сразу, хотя запись длится доли секунды.
pub fn open_readonly(db: &Path) -> Result<Connection, SqlError> {
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| SqlError::Sqlite(e.to_string()))?;
    c.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| SqlError::Sqlite(e.to_string()))?;
    юникод_регистр(&c).map_err(|e| SqlError::Sqlite(e.to_string()))?;
    Ok(c)
}

/// Подменить `lower()` и `upper()` версиями, понимающими кириллицу.
///
/// Встроенные функции SQLite меняют регистр только у ASCII:
/// `lower('ЦеныНоменклатуры')` возвращает строку как есть. Отсюда два тихих
/// промаха, оба без ошибки:
///
/// - запрос агента `lower(name) LIKE '%обеспечен%'` (так модель пишет в
///   `sql`, прогон 11) не находил ни одного русского имени;
/// - `find` сравнивал `callee_key` (понижен Rust при сборке) с
///   `lower(m.name)` — у русских имён методов совпадения не было, и сигнал
///   связности по графу был нулём (`СообщитьПользователю` в УТ: 2304
///   вызова, засчитано 0).
///
/// Подменяются функции, а не `NOCASE`: в схеме индекса `lower(` не
/// встречается, поэтому порядок индексов от подмены не зависит (подмена
/// сравнения `NOCASE` сделала бы его неверным молча). Живёт подмена только в
/// соединениях сервера; файл индекса и внешние клиенты SQLite её не видят.
fn юникод_регистр(c: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::{Context, FunctionFlags};
    use rusqlite::types::{Value as V, ValueRef};
    let флаги = FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_DETERMINISTIC
        | FunctionFlags::SQLITE_INNOCUOUS;
    // Как у встроенных: NULL остаётся NULL, число и текст — текстом.
    fn сменить(ctx: &Context<'_>, f: fn(&str) -> String) -> rusqlite::Result<V> {
        Ok(match ctx.get_raw(0) {
            ValueRef::Null => V::Null,
            ValueRef::Integer(i) => V::Text(i.to_string()),
            ValueRef::Real(r) => V::Text(r.to_string()),
            ValueRef::Text(t) => V::Text(f(&String::from_utf8_lossy(t))),
            ValueRef::Blob(b) => V::Text(f(&String::from_utf8_lossy(b))),
        })
    }
    c.create_scalar_function("lower", 1, флаги, |ctx| {
        сменить(ctx, str::to_lowercase)
    })?;
    c.create_scalar_function("upper", 1, флаги, |ctx| {
        сменить(ctx, str::to_uppercase)
    })?;
    Ok(())
}

/// Ключевые слова, которых не должно быть в read-only запросе.
///
/// `ATTACH` и `PRAGMA` — отдельно и не про запись: первый цепляет чужой файл
/// (в том числе на запись), второй меняет поведение соединения. Оба проходят
/// проверку «это же не INSERT» и потому названы поимённо.
const ЗАПРЕЩЕНО: &[&str] = &[
    "insert",
    "update",
    "delete",
    "drop",
    "create",
    "alter",
    "replace",
    "attach",
    "detach",
    "pragma",
    "vacuum",
    "reindex",
    "begin",
    "commit",
    "rollback",
    "savepoint",
];

/// Проверить, что запрос — один читающий оператор.
///
/// Возвращает нормализованный текст (без хвостовой `;`) либо причину отказа.
pub fn validate(sql: &str) -> Result<String, SqlError> {
    let голый = strip_comments(sql);
    let t = голый.trim().trim_end_matches(';').trim();

    if t.is_empty() {
        return Err(SqlError::Rejected("пустой запрос".into()));
    }

    // Один оператор: точка с запятой внутри — признак второго.
    if t.contains(';') {
        return Err(SqlError::Rejected(
            "разрешён ровно один оператор; символ ';' внутри запроса запрещён".into(),
        ));
    }

    let ниж = t.to_lowercase();
    if !(ниж.starts_with("select") || ниж.starts_with("with")) {
        return Err(SqlError::Rejected(
            "разрешены только SELECT и WITH … SELECT (соединение открыто на чтение)".into(),
        ));
    }

    // Отдельно: табличные функции `pragma_*` (pragma_table_info и родня).
    // Через них PRAGMA вызывается ИЗНУТРИ SELECT, и проверка «слово целиком»
    // их не видит — `pragma_foo` это одна лексема, а не `pragma`. Поймано
    // собственным тестом, а не рассуждением: наивная проверка пропускала.
    if ниж.contains("pragma_") {
        return Err(SqlError::Rejected(
            "табличные функции pragma_* запрещены: устройство индекса спрашивают \
             инструментом schema, а не через PRAGMA"
                .into(),
        ));
    }

    // Ищем запрещённые слова как отдельные лексемы, а не как подстроки:
    // иначе колонка `created_at` попадёт под запрет из-за `create`.
    for сл in ЗАПРЕЩЕНО {
        if содержит_слово(&ниж, сл) {
            return Err(SqlError::Rejected(format!(
                "ключевое слово '{}' запрещено в read-only запросе",
                сл.to_uppercase()
            )));
        }
    }

    Ok(t.to_string())
}

/// Есть ли в тексте слово целиком (границы — не буква, не цифра, не `_`).
fn содержит_слово(текст: &str, слово: &str) -> bool {
    let байты = текст.as_bytes();
    let мишень = слово.as_bytes();
    let гран = |c: u8| !(c.is_ascii_alphanumeric() || c == b'_');
    let mut i = 0usize;
    while i + мишень.len() <= байты.len() {
        if &байты[i..i + мишень.len()] == мишень {
            let слева = i == 0 || гран(байты[i - 1]);
            let справа = i + мишень.len() == байты.len() || гран(байты[i + мишень.len()]);
            if слева && справа {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Убрать комментарии — иначе `--` прячет от разбора что угодно.
fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut в_строке: Option<char> = None;
    while let Some(c) = chars.next() {
        // Внутри строкового литерала комментариев нет.
        if let Some(кавычка) = в_строке {
            out.push(c);
            if c == кавычка {
                в_строке = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                в_строке = Some(c);
                out.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                for c2 in chars.by_ref() {
                    if c2 == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut пред = '\0';
                for c2 in chars.by_ref() {
                    if пред == '*' && c2 == '/' {
                        break;
                    }
                    пред = c2;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Выполнить проверенный запрос и вернуть строки как JSON.
///
/// Форма ответа — колонки отдельно, строки массивами: так втрое дешевле по
/// токенам, чем повторять имена полей в каждом объекте.
pub fn run(conn: &Connection, sql: &str, limit: usize) -> Result<Value, SqlError> {
    let проверен = validate(sql)?;
    let лимит = limit.clamp(1, MAX_LIMIT);

    let mut stmt = conn
        .prepare(&проверен)
        .map_err(|e| SqlError::Sqlite(с_подсказкой(conn, &проверен, e.to_string())))?;
    let колонки: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let ширина = колонки.len();

    let mut строки: Vec<Value> = Vec::new();
    let mut усечено = false;
    let mut rows = stmt
        .query([])
        .map_err(|e| SqlError::Sqlite(e.to_string()))?;
    while let Some(r) = rows.next().map_err(|e| SqlError::Sqlite(e.to_string()))? {
        if строки.len() >= лимит {
            усечено = true;
            break;
        }
        let mut строка = Vec::with_capacity(ширина);
        for i in 0..ширина {
            строка.push(значение(r, i));
        }
        строки.push(Value::Array(строка));
    }

    let вернули = строки.len();
    Ok(json!({
        "columns": колонки,
        "rows": строки,
        "returned": вернули,
        // Честно говорим, что ответ обрезан: молча усечённая выборка читается
        // как полная и даёт неверный вывод.
        "truncated": усечено,
        "limit": лимит,
    }))
}

/// Дописать к «no such column» настоящие столбцы таблиц, названных в запросе.
///
/// Голый текст SQLite называет только промах, и агент идёт по кругу
/// `sql` → ошибка → `schema` → `sql`: в замере на прогоне вопросов это была
/// бОльшая часть ошибок инструментов. Столбцы тех же таблиц в той же ошибке
/// разрывают круг за один шаг.
///
/// Таблицы ищутся пересечением слов запроса со списком таблиц и
/// представлений индекса, а не разбором `FROM`/`JOIN`: так не теряются
/// перечисление через запятую, подзапросы и псевдонимы, а имя CTE в индексе
/// не значится и само выпадает. Лишняя таблица (слово совпало с именем
/// таблицы случайно) стоит строки в ошибке; пропущенная — ещё одного круга.
fn с_подсказкой(conn: &Connection, запрос: &str, ошибка: String) -> String {
    if ошибка.contains("no such table") {
        return с_таблицами(conn, ошибка);
    }
    if !ошибка.contains("no such column") {
        return ошибка;
    }
    let слова: std::collections::HashSet<String> = запрос
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    // Подсказка — добавка к ошибке, а не новая ошибка: сбой чтения схемы
    // даёт пустой список, исходный текст SQLite остаётся в начале ответа.
    let имена = |q: &str, p: &[&dyn rusqlite::ToSql]| -> Vec<String> {
        conn.prepare(q)
            .and_then(|mut st| {
                st.query_map(p, |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default()
    };
    let таблицы: Vec<String> = таблицы_индекса(conn)
        .into_iter()
        .filter(|t| слова.contains(&t.to_lowercase()))
        .collect();
    if таблицы.is_empty() {
        return format!(
            "{ошибка}. Таблицы индекса в запросе не распознаны — их столбцы покажет инструмент schema"
        );
    }
    let описи: Vec<String> = таблицы
        .iter()
        .map(|t| {
            let столбцы = имена("SELECT name FROM pragma_table_info(?1)", &[t]);
            format!("{t}({})", столбцы.join(", "))
        })
        .collect();
    format!(
        "{ошибка}. Настоящие столбцы таблиц запроса: {}",
        описи.join("; ")
    )
}

/// Дописать к «no such table» таблицы индекса — сначала похожие на промах.
///
/// Тот же круг, что у столбцов: на пробе 11.11.2026 модель написала
/// `FROM movements` по имени инструмента, а таблица — `register_movements`.
/// Похожие — те, чьё имя содержит промах или (от четырёх знаков) содержится
/// в нём, без учёта регистра; они идут первыми, чтобы исправление читалось
/// сразу. Служебные таблицы SQLite и теневые таблицы полнотекстового поиска не
/// перечисляются: запрос к ним агенту не нужен, а список они удлиняют.
fn с_таблицами(conn: &Connection, ошибка: String) -> String {
    let промах = ошибка
        .split("no such table:")
        .nth(1)
        .map(|s| {
            s.trim()
                .trim_matches(|c| c == '"' || c == '`')
                .to_lowercase()
        })
        .unwrap_or_default();
    // Имя может прийти со схемой (`main.x`) — сравнивается последняя часть.
    let промах = промах.rsplit('.').next().unwrap_or_default().to_string();
    let все = таблицы_индекса(conn);
    if все.is_empty() {
        return ошибка;
    }
    let похож = |t: &str| {
        let t = t.to_lowercase();
        // Обратное вхождение — только для имён от четырёх знаков: иначе
        // короткое имя вроде `t` «похоже» на любой промах, где есть эта буква.
        !промах.is_empty()
            && (t.contains(&промах) || (t.chars().count() >= 4 && промах.contains(&t)))
    };
    let похожие: Vec<&str> = все
        .iter()
        .map(String::as_str)
        .filter(|t| похож(t))
        .collect();
    let начало = if похожие.is_empty() {
        String::new()
    } else {
        format!(" Похожие: {}.", похожие.join(", "))
    };
    format!(
        "{ошибка}.{начало} Таблицы индекса: {} — столбцы покажет инструмент schema",
        все.join(", ")
    )
}

/// Таблицы и представления индекса — без служебных SQLite и теневых FTS.
/// Сбой чтения схемы даёт пустой список: подсказка — добавка к ошибке, а не
/// новая ошибка.
fn таблицы_индекса(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT name FROM sqlite_master WHERE type IN ('table','view') \
         AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
    )
    .and_then(|mut st| {
        st.query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
    })
    .unwrap_or_default()
    .into_iter()
    .filter(|t| !теневая_fts(t))
    .collect()
}

/// Теневая таблица FTS5 (`<имя>_fts_data`, `_idx`, `_content`, `_docsize`,
/// `_config`) — внутреннее устройство поиска, не данные.
fn теневая_fts(t: &str) -> bool {
    ["_data", "_idx", "_content", "_docsize", "_config"]
        .iter()
        .any(|суф| t.ends_with(суф) && t.contains("_fts"))
}

fn значение(r: &rusqlite::Row<'_>, i: usize) -> Value {
    use rusqlite::types::ValueRef;
    match r.get_ref(i) {
        Ok(ValueRef::Null) => Value::Null,
        Ok(ValueRef::Integer(v)) => json!(v),
        Ok(ValueRef::Real(v)) => json!(v),
        Ok(ValueRef::Text(v)) => json!(String::from_utf8_lossy(v)),
        // Блобы наружу не отдаём: это векторы семантики, агенту они не нужны,
        // а контекст выносят мгновенно.
        Ok(ValueRef::Blob(b)) => json!(format!("<blob {} байт>", b.len())),
        Err(e) => json!(format!("<ошибка чтения: {e}>")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn пропускает_обычную_выборку() {
        assert!(validate("SELECT name FROM modules LIMIT 10").is_ok());
        assert!(validate("  select 1  ;  ").is_ok());
        assert!(validate("WITH x AS (SELECT 1 a) SELECT a FROM x").is_ok());
    }

    #[test]
    fn отвергает_запись() {
        for q in [
            "INSERT INTO modules VALUES (1)",
            "UPDATE modules SET name='x'",
            "DELETE FROM modules",
            "DROP TABLE modules",
        ] {
            assert!(validate(q).is_err(), "должно быть отклонено: {q}");
        }
    }

    #[test]
    fn отвергает_attach_и_pragma() {
        // Не про запись, но опаснее прочего: ATTACH цепляет чужой файл,
        // PRAGMA меняет поведение соединения. Оба проходят наивную проверку.
        assert!(validate("SELECT 1; ATTACH DATABASE 'x' AS y").is_err());
        assert!(validate("SELECT * FROM x WHERE 1=1 AND pragma_foo()").is_err());
    }

    #[test]
    fn отвергает_второй_оператор() {
        assert!(validate("SELECT 1; SELECT 2").is_err());
    }

    #[test]
    fn комментарий_не_прячет_запрещённое() {
        // Наивная проверка «начинается с SELECT» пропустила бы это.
        let r = validate("SELECT 1 -- безобидно\n; DROP TABLE modules");
        assert!(r.is_err(), "запрещённое за комментарием должно ловиться");
    }

    #[test]
    fn слово_целиком_а_не_подстрока() {
        // `created_at` содержит `create`, но запросом на создание не является.
        assert!(
            validate("SELECT created_at FROM modules").is_ok(),
            "колонка с подстрокой create должна проходить"
        );
        assert!(validate("SELECT updated_by FROM modules").is_ok());
    }

    #[test]
    fn строковый_литерал_не_ломает_разбор_комментариев() {
        // Дефисы внутри строки — не комментарий.
        assert!(validate("SELECT * FROM modules WHERE name = 'a--b'").is_ok());
    }

    fn база() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE t (a INTEGER, b TEXT);
             INSERT INTO t VALUES (1,'один'),(2,'два'),(3,'три');",
        )
        .unwrap();
        c
    }

    #[test]
    fn выполняет_и_отдаёт_колонки_и_строки() {
        let c = база();
        let v = run(&c, "SELECT a, b FROM t ORDER BY a", 10).unwrap();
        assert_eq!(v["columns"], json!(["a", "b"]));
        assert_eq!(v["returned"], 3);
        assert_eq!(v["truncated"], false);
        assert_eq!(v["rows"][0], json!([1, "один"]));
    }

    #[test]
    fn усечение_названо_вслух() {
        let c = база();
        let v = run(&c, "SELECT a FROM t ORDER BY a", 2).unwrap();
        assert_eq!(v["returned"], 2);
        assert_eq!(
            v["truncated"], true,
            "обрезанная выборка обязана сказать это"
        );
    }

    #[test]
    fn ошибка_sqlite_доходит_текстом() {
        let c = база();
        match run(&c, "SELECT нет_такой_колонки FROM t", 10) {
            Err(SqlError::Sqlite(s)) => assert!(!s.is_empty()),
            other => panic!("ожидалась ошибка SQLite, получено {other:?}"),
        }
    }

    #[test]
    fn нет_столбца_ошибка_называет_настоящие() {
        let c = база();
        c.execute_batch("CREATE TABLE u (id INTEGER, t_a INTEGER, имя TEXT);")
            .unwrap();
        // Псевдоним и соединение: столбцы обеих таблиц, и только их.
        let q = "SELECT x.нет FROM t x JOIN u ON u.t_a = x.a";
        match run(&c, q, 10) {
            Err(SqlError::Sqlite(s)) => {
                assert!(s.contains("no such column"), "{s}");
                assert!(s.contains("t(a, b)"), "{s}");
                assert!(s.contains("u(id, t_a, имя)"), "{s}");
            }
            other => panic!("ожидалась ошибка SQLite, получено {other:?}"),
        }
        // Таблица, не названная в запросе, в подсказку не попадает.
        match run(&c, "SELECT нет FROM t", 10) {
            Err(SqlError::Sqlite(s)) => {
                assert!(s.contains("t(a, b)"), "{s}");
                assert!(!s.contains("u("), "лишняя таблица: {s}");
            }
            other => panic!("ожидалась ошибка SQLite, получено {other:?}"),
        }
    }

    #[test]
    fn нет_таблицы_ошибка_называет_таблицы_и_похожие_первыми() {
        let c = база();
        c.execute_batch(
            "CREATE TABLE register_movements (doc TEXT, reg TEXT);
             CREATE VIRTUAL TABLE methods_fts USING fts5(name);",
        )
        .unwrap();
        // Промах пробы 11.11: имя инструмента вместо имени таблицы.
        match run(&c, "SELECT * FROM movements", 10) {
            Err(SqlError::Sqlite(s)) => {
                assert!(s.contains("no such table"), "{s}");
                assert!(s.contains("Похожие: register_movements."), "{s}");
                // Перечень — между «Таблицы индекса:» и « — », через запятую.
                let перечень: Vec<&str> = s
                    .split("Таблицы индекса:")
                    .nth(1)
                    .and_then(|x| x.split(" — ").next())
                    .expect("перечень таблиц")
                    .split(',')
                    .map(str::trim)
                    .collect();
                assert_eq!(
                    перечень,
                    ["methods_fts", "register_movements", "t"],
                    "все таблицы, без теневых FTS и служебных SQLite: {s}"
                );
            }
            other => panic!("ожидалась ошибка SQLite, получено {other:?}"),
        }
        // Похожих нет — список всё равно есть, без раздела «Похожие».
        match run(&c, "SELECT * FROM совсем_другое", 10) {
            Err(SqlError::Sqlite(s)) => {
                assert!(!s.contains("Похожие"), "{s}");
                assert!(s.contains("register_movements"), "{s}");
            }
            other => panic!("ожидалась ошибка SQLite, получено {other:?}"),
        }
    }

    #[test]
    fn нет_столбца_в_cte_отсылает_к_schema() {
        // Имя CTE в индексе не значится — честно говорим, что таблиц не узнали.
        let c = база();
        match run(&c, "WITH w AS (SELECT 1 k) SELECT нет FROM w", 10) {
            Err(SqlError::Sqlite(s)) => assert!(s.contains("schema"), "{s}"),
            other => panic!("ожидалась ошибка SQLite, получено {other:?}"),
        }
    }

    #[test]
    fn прочие_ошибки_без_подсказки() {
        let c = база();
        match run(&c, "SELECT a FROM нет_таблицы", 10) {
            Err(SqlError::Sqlite(s)) => assert!(!s.contains("Настоящие столбцы"), "{s}"),
            other => panic!("ожидалась ошибка SQLite, получено {other:?}"),
        }
    }

    #[test]
    fn readonly_соединение_не_даёт_писать() {
        // Главный рубеж: проверяется на настоящем файле, а не на разборе текста.
        let dir = std::env::temp_dir().join("gyrfalcon-sql-ro-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        let ro = open_readonly(&db).unwrap();
        assert!(
            ro.execute_batch("INSERT INTO t VALUES (2)").is_err(),
            "запись обязана падать на уровне соединения, а не разбора"
        );
    }

    #[test]
    fn lower_и_upper_понимают_кириллицу() {
        let dir = std::env::temp_dir().join(format!("gyrfalcon-sql-lower-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch(
                "CREATE TABLE modules (id INTEGER, rel_path TEXT);
                 CREATE TABLE methods (module_id INTEGER, name TEXT);
                 CREATE TABLE calls (callee_key TEXT);
                 INSERT INTO modules VALUES (1, 'CommonModules/Мод/Ext/Module.bsl');
                 INSERT INTO methods VALUES (1, 'СообщитьПользователю');
                 INSERT INTO calls VALUES ('CommonModules/Мод/Ext/Module.bsl::сообщитьпользователю');
                 INSERT INTO calls VALUES ('CommonModules/Мод/Ext/Module.bsl::сообщитьпользователю');",
            )
            .unwrap();
            // Встроенная — только ASCII: ровно то, что подменяется.
            let было: String = c
                .query_row("SELECT lower('ЦеныНоменклатуры')", [], |r| r.get(0))
                .unwrap();
            assert_eq!(было, "ЦеныНоменклатуры");
        }
        let ro = open_readonly(&db).unwrap();
        let v = run(
            &ro,
            "SELECT lower('ЦеныНоменклатуры'), upper('ценыNom'), lower(NULL), lower(42)",
            10,
        )
        .unwrap();
        assert_eq!(
            v["rows"][0],
            json!(["ценыноменклатуры", "ЦЕНЫNOM", null, "42"])
        );
        // Как пишет запросы модель: lower(...) LIKE по русской подстроке.
        let v = run(
            &ro,
            "SELECT name FROM methods WHERE lower(name) LIKE '%пользовател%'",
            10,
        )
        .unwrap();
        assert_eq!(v["returned"], 1, "{v}");
        // Связность в `find`: ключ из Rust против lower() в SQL.
        let deg: i64 = ro
            .query_row(
                "SELECT (SELECT COUNT(*) FROM calls c
                          WHERE c.callee_key = mo.rel_path || '::' || lower(m.name))
                 FROM methods m JOIN modules mo ON mo.id = m.module_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deg, 2, "вызовы русского метода должны засчитываться");
        drop(ro);
        let _ = std::fs::remove_dir_all(dir);
    }
}
