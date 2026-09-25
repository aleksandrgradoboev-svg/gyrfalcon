//! Единый локальный индекс официальных стандартов 1С для всех проектов.
//! Корпус хранится один раз рядом с индексами, а не внутри конфигураций.

use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::PathBuf;

const DB_NAME: &str = "shared-v8std.sqlite";

fn database_path(index: &Connection) -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("GYRFALCON_V8STD_DB") {
        return Ok(PathBuf::from(explicit));
    }
    let mut statement = index
        .prepare("PRAGMA database_list")
        .map_err(|e| format!("не определён каталог индекса: {e}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .map_err(|e| format!("не определён каталог индекса: {e}"))?;
    for row in rows {
        let (name, path) = row.map_err(|e| e.to_string())?;
        if name == "main" && !path.is_empty() {
            return Ok(PathBuf::from(path)
                .parent()
                .ok_or("у индекса нет родительского каталога")?
                .join(DB_NAME));
        }
    }
    Err("общему базису нужен файловый индекс или GYRFALCON_V8STD_DB".into())
}

fn open_shared(index: &Connection) -> Result<Connection, String> {
    let path = database_path(index)?;
    if !path.is_file() {
        return Err(format!(
            "общий базис 1С не собран: {}. Выполните tools/v8std-build.py после каталога и аудита",
            path.display()
        ));
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("не открыт общий базис 1С: {e}"))
}

pub fn stats(index: &Connection) -> Value {
    let Ok(shared) = open_shared(index) else {
        return json!({"status":"missing","shared":true,"project_copy":false});
    };
    let read = |key: &str| -> Option<String> {
        shared
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .ok()
    };
    let articles = read("article_count").and_then(|v| v.parse::<usize>().ok());
    let clauses = read("clause_count").and_then(|v| v.parse::<usize>().ok());
    match (articles, clauses) {
        (Some(articles), Some(clauses)) => json!({"status":"ready","shared":true,
            "project_copy":false,"source":read("source"),
            "articles":articles,"addressable_blocks":clauses,
            "audit_sha256":read("audit_sha256"),
            "coverage_kind":read("coverage_kind")}),
        _ => json!({"status":"invalid","shared":true,"project_copy":false}),
    }
}

fn fts_terms(input: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    for word in input.split(|c: char| !c.is_alphanumeric()) {
        if word.chars().count() >= 3 {
            let word = word.to_lowercase();
            if !words.contains(&word) {
                words.push(word);
            }
        }
    }
    if words.is_empty() {
        return Err("стандарты 1С: запрос должен содержать слово длиной от трёх символов".into());
    }
    Ok(words
        .into_iter()
        .map(|word| {
            let chars: Vec<_> = word.chars().collect();
            let keep = if chars.len() >= 7 {
                chars.len() - 2
            } else {
                chars.len()
            };
            format!("\"{}\"*", chars[..keep].iter().collect::<String>())
        })
        .collect())
}

pub fn search(index: &Connection, args: &Value) -> Result<Value, String> {
    let project = args
        .get("project")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && *s != "*")
        .ok_or("standards_1c требует одно явное имя project из list_projects")?;
    let shared = open_shared(index)?;
    match args.get("view").and_then(Value::as_str).unwrap_or("search") {
        "catalog" => return catalog(index, &shared, project, args),
        "article" => return article(index, &shared, project, args),
        "clause" => return clause(index, &shared, project, args),
        "practices" => return practices(index, project, args),
        "search" => {}
        _ => {
            return Err("standards_1c view: search, catalog, article, clause или practices".into())
        }
    }
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or("standards_1c view=search требует query")?;
    let terms = fts_terms(query)?;
    let expression = terms.join(" AND ");
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(8)
        .clamp(1, 30);
    let mut statement = shared
        .prepare(
            "SELECT c.id,a.id,a.title,c.number,a.section,a.source_url,
                substr(c.text,1,1800),length(c.text),
                snippet(clause_fts,3,'[',']',' … ',32)
         FROM clause_fts JOIN clauses c ON c.id=clause_fts.id
         JOIN articles a ON a.id=c.article_id
         WHERE clause_fts MATCH ?1
         ORDER BY bm25(clause_fts,0.0,5.0,2.0,1.0) LIMIT ?2",
        )
        .map_err(|e| format!("не подготовлен поиск стандартов: {e}"))?;
    let mut collect = |expression: &str| -> Result<Vec<Value>, String> {
        statement
            .query_map(params![expression, limit], |row| {
                Ok(json!({"clause_id":row.get::<_, String>(0)?,
                "article_id":row.get::<_, String>(1)?,
                "title":row.get::<_, String>(2)?,
                "number":row.get::<_, Option<String>>(3)?,
                "section":row.get::<_, String>(4)?,
                "url":row.get::<_, String>(5)?,
                "text":row.get::<_, String>(6)?,
                "text_length":row.get::<_, i64>(7)?,
                "snippet":row.get::<_, String>(8)?}))
            })
            .map_err(|e| format!("ошибка поиска стандартов: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("не прочитан результат стандартов: {e}"))
    };
    let mut hits = collect(&expression)?;
    let mut mode = "all_terms";
    if hits.is_empty() && terms.len() > 1 {
        hits = collect(&terms.join(" OR "))?;
        mode = "fallback_any_term";
    }
    let suppressed = project_suppressions(index, project)?;
    for hit in &mut hits {
        let related = matching_overrides(
            &suppressed,
            hit["url"].as_str().unwrap_or(""),
            hit["number"].as_str(),
        );
        if !related.is_empty() {
            hit["project_override"] = json!({"status":"suppressed","by":related});
        }
    }
    Ok(
        json!({"source":"shared official 1C standards","project":project,
        "query":query,"match_mode":mode,"baseline":stats(index),
        "returned":hits.len(),"rules":hits,"suppressed_curated_standards":suppressed,
        "note":"Это исходные пункты общего справочника, а не автоматически принятые проектные практики. Точные совпадения с переопределёнными нормализованными правилами помечены project_override; остальные исключения проверяйте через project_practices."}),
    )
}

fn practices(index: &Connection, project: &str, args: &Value) -> Result<Value, String> {
    let rules = crate::project_practices::standard_baseline()?;
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let rule_id = args.get("rule_id").and_then(Value::as_str);
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 50) as usize;
    let suppressed = project_suppressions(index, project)?;
    let matches: Vec<Value> = rules
        .into_iter()
        .filter(|rule| {
            if rule_id.is_some_and(|id| rule["id"] != id) {
                return false;
            }
            let searchable = format!(
                "{} {} {} {} {}",
                rule["id"].as_str().unwrap_or(""),
                rule["title"].as_str().unwrap_or(""),
                rule["guidance"].as_str().unwrap_or(""),
                rule["source"]["scope"].as_str().unwrap_or(""),
                rule["source"]["anchor"].as_str().unwrap_or(""),
            )
            .to_lowercase();
            terms.iter().all(|term| searchable.contains(term))
        })
        .collect();
    let total = matches.len();
    let page: Vec<Value> = matches
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|mut rule| {
            let override_by: Vec<Value> = suppressed
                .iter()
                .filter(|item| item["standard_id"] == rule["id"])
                .map(|item| item["by"].clone())
                .collect();
            if !override_by.is_empty() {
                rule["project_override"] = json!({"status":"suppressed","by":override_by});
            }
            rule
        })
        .collect();
    let next_offset = offset.saturating_add(page.len());
    Ok(
        json!({"view":"practices","project":project,"baseline":stats(index),
        "query":query,"rule_id":rule_id,"total":total,"offset":offset,
        "returned":page.len(),"next_offset":next_offset,"has_more":next_offset<total,
        "rules":page,"note":"Принятые общие правила; для полного текста и исключений сверяйте источник по source.anchor через view=clause."}),
    )
}

fn catalog(
    index: &Connection,
    shared: &Connection,
    project: &str,
    args: &Value,
) -> Result<Value, String> {
    let offset = args
        .get("offset")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(30)
        .clamp(1, 50);
    let mut statement = shared.prepare(
        "SELECT id,title,section,source_url,clause_count FROM articles ORDER BY CAST(id AS INTEGER) LIMIT ?1 OFFSET ?2"
    ).map_err(|e| format!("не подготовлен каталог стандартов: {e}"))?;
    let articles = statement
        .query_map(params![limit, offset], |row| {
            Ok(json!({
                "article_id":row.get::<_, String>(0)?, "title":row.get::<_, String>(1)?,
                "section":row.get::<_, String>(2)?, "url":row.get::<_, String>(3)?,
                "blocks":row.get::<_, i64>(4)?
            }))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let total: i64 = shared
        .query_row("SELECT count(*) FROM articles", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let next_offset = offset + articles.len() as i64;
    Ok(
        json!({"view":"catalog","project":project,"baseline":stats(index),
        "offset":offset,"returned":articles.len(),"articles":articles,
        "total":total,"next_offset":next_offset,"has_more":next_offset<total}),
    )
}

fn article(
    index: &Connection,
    shared: &Connection,
    project: &str,
    args: &Value,
) -> Result<Value, String> {
    let id = args
        .get("article_id")
        .and_then(Value::as_str)
        .ok_or("standards_1c view=article требует article_id")?;
    let meta: (String, String, String) = shared
        .query_row(
            "SELECT title,section,source_url FROM articles WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| format!("статья {id} не найдена"))?;
    let mut statement = shared.prepare(
        "SELECT id,number,length(text),substr(text,1,240) FROM clauses WHERE article_id=?1 ORDER BY ordinal"
    ).map_err(|e| e.to_string())?;
    let blocks = statement
        .query_map([id], |row| {
            Ok(json!({
                "clause_id":row.get::<_, String>(0)?,"number":row.get::<_, Option<String>>(1)?,
                "text_length":row.get::<_, i64>(2)?,"preview":row.get::<_, String>(3)?
            }))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"view":"article","project":project,"baseline":stats(index),
        "article_id":id,"title":meta.0,"section":meta.1,"url":meta.2,
        "blocks":blocks,"suppressed_curated_standards":project_suppressions(index, project)?}),
    )
}

fn clause(
    index: &Connection,
    shared: &Connection,
    project: &str,
    args: &Value,
) -> Result<Value, String> {
    let id = args
        .get("clause_id")
        .and_then(Value::as_str)
        .ok_or("standards_1c view=clause требует clause_id")?;
    let offset = args
        .get("offset")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let max_chars = args
        .get("max_chars")
        .and_then(Value::as_i64)
        .unwrap_or(4000)
        .clamp(1, 8000);
    let (article_id,title,number,section,url,text_length,body):
        (String,String,Option<String>,String,String,i64,String) = shared.query_row(
        "SELECT a.id,a.title,c.number,a.section,a.source_url,length(c.text),substr(c.text,?2+1,?3)
         FROM clauses c JOIN articles a ON a.id=c.article_id WHERE c.id=?1",
        params![id,offset,max_chars],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))
    ).map_err(|_| format!("пункт {id} не найден"))?;
    let next_offset = offset + body.chars().count() as i64;
    let suppressed = project_suppressions(index, project)?;
    let override_by = matching_overrides(&suppressed, &url, number.as_deref());
    Ok(
        json!({"view":"clause","project":project,"baseline":stats(index),
        "clause_id":id,"article_id":article_id,"title":title,"number":number,
        "section":section,"url":url,"offset":offset,"text":body,
        "text_length":text_length,"next_offset":next_offset,"has_more":next_offset<text_length,
        "project_override":override_by,"suppressed_curated_standards":suppressed}),
    )
}

fn project_suppressions(index: &Connection, project: &str) -> Result<Vec<Value>, String> {
    let accepted = crate::project_practices::accepted_normative_rules(index, project)?;
    crate::project_practices::suppressed_standards(&accepted)
}

fn matching_overrides(suppressed: &[Value], url: &str, number: Option<&str>) -> Vec<Value> {
    let number = number.unwrap_or("").trim_end_matches('.');
    suppressed
        .iter()
        .filter(|item| item["standard_url"] == url && item["standard_section"] == number)
        .map(|item| item["by"].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_uses_single_sibling_database() {
        let root = std::env::temp_dir().join(format!("gyrfalcon-v8std-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let index = Connection::open(root.join("pilot.db")).unwrap();
        index
            .execute_batch("CREATE TABLE index_meta(key TEXT,value TEXT)")
            .unwrap();
        index
            .execute(
                "INSERT INTO index_meta VALUES('source_path',?1)",
                [root.to_string_lossy().as_ref()],
            )
            .unwrap();
        let profile = root.join(".gyrfalcon/project-practices.json");
        std::fs::create_dir_all(profile.parent().unwrap()).unwrap();
        let original = r#"{"version":1,"project":"pilot","source_commit":"old",
            "rules":[{"id":"preserve-old-code-and-mark-changes","title":"Customer",
            "guidance":"Preserve old code","status":"verified","legacy":false,
            "source":{"kind":"manual"},"approval":{"kind":"explicit",
            "authority":"customer","reviewer":"user","reason":"contract"}}]}"#;
        std::fs::write(&profile, original).unwrap();
        let shared = Connection::open(root.join(DB_NAME)).unwrap();
        shared.execute_batch("CREATE TABLE meta(key TEXT,value TEXT);
            INSERT INTO meta VALUES('article_count','1');
            INSERT INTO meta VALUES('clause_count','1');
            CREATE TABLE articles(id TEXT,title TEXT,section TEXT,source_url TEXT,clause_count INTEGER);
            CREATE TABLE clauses(id TEXT,article_id TEXT,number TEXT,text TEXT,ordinal INTEGER);
            CREATE VIRTUAL TABLE clause_fts USING fts5(id UNINDEXED,title,section,text);
            INSERT INTO articles VALUES('456','Тексты модулей','Код','https://its.1c.ru/db/content/v8std/src/400/100/i8100456.htm',1);
            INSERT INTO clauses VALUES('456:3','456','3.','Не оставлять закомментированный код',1);
            INSERT INTO clause_fts VALUES('456:3','Тексты модулей','Код','Не оставлять закомментированный код');").unwrap();
        drop(shared);
        let got = search(
            &index,
            &json!({"project":"pilot","query":"закомментированный код"}),
        )
        .unwrap();
        assert_eq!(got["returned"], 1);
        assert_eq!(got["rules"][0]["clause_id"], "456:3");
        assert_eq!(got["baseline"]["articles"], 1);
        assert_eq!(got["rules"][0]["project_override"]["status"], "suppressed");
        assert_eq!(
            got["rules"][0]["project_override"]["by"][0],
            "preserve-old-code-and-mark-changes"
        );
        let catalog = search(&index, &json!({"project":"pilot","view":"catalog"})).unwrap();
        assert_eq!(catalog["articles"][0]["article_id"], "456");
        let article = search(
            &index,
            &json!({"project":"pilot","view":"article","article_id":"456"}),
        )
        .unwrap();
        assert_eq!(article["blocks"][0]["clause_id"], "456:3");
        let clause = search(
            &index,
            &json!({"project":"pilot","view":"clause","clause_id":"456:3","max_chars":10}),
        )
        .unwrap();
        assert_eq!(
            clause["project_override"][0],
            "preserve-old-code-and-mark-changes"
        );
        assert_eq!(clause["has_more"], true);
        assert_eq!(clause["next_offset"], 10);
        let accepted = search(
            &index,
            &json!({"project":"pilot","view":"practices","rule_id":"std-456-no-commented-code"}),
        )
        .unwrap();
        assert_eq!(accepted["returned"], 1);
        assert_eq!(
            accepted["rules"][0]["project_override"]["status"],
            "suppressed"
        );
        let page = search(
            &index,
            &json!({"project":"pilot","view":"practices","limit":2}),
        )
        .unwrap();
        assert_eq!(page["returned"], 2);
        assert_eq!(page["has_more"], true);
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), original);
        drop(index);
        std::fs::remove_dir_all(root).unwrap();
    }
}
