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
pub fn open_readonly(db: &Path) -> Result<Connection, SqlError> {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| SqlError::Sqlite(e.to_string()))
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
        .map_err(|e| SqlError::Sqlite(e.to_string()))?;
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
}
