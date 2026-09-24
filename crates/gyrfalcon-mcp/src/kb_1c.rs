//! Read-only поиск по встроенной справке выбранного проектного индекса.

use rusqlite::{params_from_iter, types::Value as SqlValue, Connection};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const MAX_CANDIDATES: usize = 400;
const FULL_TEXT_CHARS: usize = 8_000;
const STOP: &[&str] = &[
    "почему",
    "как",
    "что",
    "где",
    "когда",
    "какие",
    "нужны",
    "нужно",
    "если",
    "для",
    "при",
    "это",
    "или",
    "все",
    "чтобы",
    "работает",
    "работать",
    "типовой",
];

pub fn search(conn: &Connection, args: &Value) -> Result<Value, String> {
    let project = args
        .get("project")
        .and_then(Value::as_str)
        .filter(|p| !p.trim().is_empty() && *p != "*")
        .ok_or("kb_1c требует одно явное имя project из list_projects")?;
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("нужен параметр query")?;
    let source = optional(args, "source_id");
    let version = optional(args, "config_version");
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, 20))
        .unwrap_or(8);
    let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
    let terms = terms(query);
    if terms.is_empty() {
        return Err("в запросе нет значимых слов для поиска по KB-1С".into());
    }

    let mut sql = String::from(
        "SELECT s.source_id,s.config_version,p.category,p.object_name,p.form_name,
         p.title,p.path,p.text,p.sha256,p.chars
         FROM help_fts JOIN help_pages p ON p.id=help_fts.rowid
         JOIN help_sources s ON s.source_id=p.source_id
         WHERE help_fts MATCH ?1",
    );
    let fts = terms
        .iter()
        .map(|w| format!("\"{w}\"*"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut params = vec![SqlValue::Text(fts)];
    if let Some(source_id) = source {
        params.push(SqlValue::Text(source_id.to_owned()));
        sql.push_str(&format!(" AND p.source_id=?{}", params.len()));
    }
    if let Some(ver) = version {
        params.push(SqlValue::Text(ver.to_owned()));
        sql.push_str(&format!(" AND s.config_version=?{}", params.len()));
    }
    params.push(SqlValue::Integer(MAX_CANDIDATES as i64));
    sql.push_str(&format!(" ORDER BY bm25(help_fts) LIMIT ?{}", params.len()));
    let mut stmt = conn.prepare(&sql).map_err(|e| {
        format!("справка этого проекта не проиндексирована или повреждена: {e}; перестройте индекс")
    })?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), |r| {
            let text: String = r.get(7)?;
            let sha: String = r.get(8)?;
            let item = json!({
                "source_id": r.get::<_, String>(0)?,
                "config_version": r.get::<_, String>(1)?,
                "category": r.get::<_, String>(2)?,
                "object": r.get::<_, String>(3)?,
                "form": r.get::<_, String>(4)?,
                "title": r.get::<_, String>(5)?,
                "source_path": r.get::<_, String>(6)?,
                "chars": r.get::<_, i64>(9)?,
            });
            Ok((item, text, sha))
        })
        .map_err(|e| format!("поиск KB-1С не выполнен: {e}"))?;

    let mut items: Vec<Value> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let min_matches = terms.len().min(2);
    for row in rows {
        let (mut item, text, sha) = row.map_err(|e| e.to_string())?;
        let haystack = format!("{} {}", item["title"].as_str().unwrap_or(""), text).to_lowercase();
        if terms
            .iter()
            .filter(|term| haystack.contains(term.as_str()))
            .count()
            < min_matches
        {
            continue;
        }
        let dedup_key = format!("{}:{}", item["config_version"], sha);
        if let Some(&index) = seen.get(&dedup_key) {
            let alias = json!({
                "source_id": item["source_id"],
                "title": item["title"],
                "source_path": item["source_path"],
                "category": item["category"],
                "object": item["object"],
                "form": item["form"],
            });
            items[index]["alternate_sources"]
                .as_array_mut()
                .expect("alternate_sources is an array")
                .push(alias);
            continue;
        }
        seen.insert(dedup_key, items.len());
        item["excerpt"] = Value::String(text.chars().take(700).collect());
        item["alternate_sources"] = json!([]);
        if full {
            item["text"] = Value::String(text.chars().take(FULL_TEXT_CHARS).collect());
            item["text_truncated"] = Value::Bool(text.chars().count() > FULL_TEXT_CHARS);
        }
        items.push(item);
    }
    items.truncate(limit);
    Ok(json!({
        "source": "встроенная справка Ext/Help", "read_only": true,
        "project": project, "query": query, "source_id": source,
        "config_version": version, "no_results": items.is_empty(), "items": items,
        "note": "Ответ относится только к индексу указанного проекта. Ноль по словам не доказывает отсутствие механизма; справка не описывает состояние информационной базы."
    }))
}

fn optional<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|word| {
            let word = word.to_lowercase();
            let len = word.chars().count();
            if len < 3 || STOP.contains(&word.as_str()) {
                return None;
            }
            let prefix: String = word
                .chars()
                .take(if len > 5 { len - 2 } else { len })
                .collect();
            seen.insert(prefix.clone()).then_some(prefix)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn проект_обязателен_а_результат_содержит_адрес_и_версию() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE help_sources(source_id TEXT PRIMARY KEY,root TEXT,config_version TEXT);
            CREATE TABLE help_pages(id INTEGER PRIMARY KEY,source_id TEXT,path TEXT,category TEXT,object_name TEXT,form_name TEXT,title TEXT,text TEXT,chars INTEGER,sha256 TEXT);
            CREATE VIRTUAL TABLE help_fts USING fts5(title,object_name,text,content='help_pages',content_rowid='id');
            INSERT INTO help_sources VALUES('primary','X','3.1');
            INSERT INTO help_pages VALUES(1,'primary','Documents/Отпуск/Ext/Help/ru.html','Documents','Отпуск','','Отпуск','Оформление отпуска сотрудника через документ отпуск.',52,'hash');
            INSERT INTO help_fts(rowid,title,object_name,text) VALUES(1,'Отпуск','Отпуск','Оформление отпуска сотрудника через документ отпуск.');").unwrap();
        assert!(search(&conn, &json!({"query":"отпуск"})).is_err());
        let found = search(
            &conn,
            &json!({"project":"zup","query":"оформление отпуска"}),
        )
        .unwrap();
        assert_eq!(found["project"], "zup");
        assert_eq!(found["items"][0]["config_version"], "3.1");
        assert_eq!(
            found["items"][0]["source_path"],
            "Documents/Отпуск/Ext/Help/ru.html"
        );
    }

    #[test]
    fn однострочная_полезная_справка_не_теряется() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE help_sources(source_id TEXT PRIMARY KEY,root TEXT,config_version TEXT);
            CREATE TABLE help_pages(id INTEGER PRIMARY KEY,source_id TEXT,path TEXT,category TEXT,object_name TEXT,form_name TEXT,title TEXT,text TEXT,chars INTEGER,sha256 TEXT);
            CREATE VIRTUAL TABLE help_fts USING fts5(title,object_name,text,content='help_pages',content_rowid='id');
            INSERT INTO help_sources VALUES('primary','X','1.0');
            INSERT INTO help_pages VALUES(1,'primary','DataProcessors/Test/Ext/Help/ru.html','DataProcessors','Test','','Настройка прав позволяет ограничить доступ к документам.','Настройка прав позволяет ограничить доступ к документам.',58,'hash');
            INSERT INTO help_fts(rowid,title,object_name,text) VALUES(1,'Настройка прав позволяет ограничить доступ к документам.','Test','Настройка прав позволяет ограничить доступ к документам.');").unwrap();
        let found = search(&conn, &json!({"project":"one","query":"настройка прав"})).unwrap();
        assert_eq!(found["items"].as_array().unwrap().len(), 1);
    }
}
