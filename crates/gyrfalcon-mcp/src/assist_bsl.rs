//! Помощник написания BSL: подбирает факты ДО генерации, а не только
//! проверяет готовый текст после неё.
//!
//! Инструмент намеренно широкий. Слабая модель плохо строит цепочку
//! `find -> object -> callers -> read -> check_bsl`, поэтому один вызов
//! возвращает методы, объекты, короткие места употребления, дополнения и
//! диагностику имеющегося черновика.
//!
//! Сам инструмент код за модель не сочиняет: генерация остаётся у клиента,
//! а здесь только детерминированные сведения из индекса и безопасный каркас.

use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 20;
const ATTR_LIMIT: i64 = 12;
const EXAMPLE_CHARS: usize = 600;
/// Слабее этого совпадение по смыслу в подсказки не попадает.
/// Нормированный `score` брать нельзя: лучший из любого мусора всегда 1.0.
const MIN_SEMANTIC_RAW: f32 = 0.35;
const MIN_LEXICAL: usize = 4;

#[derive(Debug, Clone)]
struct MethodCandidate {
    id: i64,
    name: String,
    kind: String,
    params: String,
    export: bool,
    line: i64,
    path: String,
    object: String,
    module_type: String,
    category: String,
    score: f32,
    semantic: f32,
    lexical: usize,
}

#[derive(Debug, Clone)]
struct ObjectCandidate {
    id: i64,
    name: String,
    category: String,
    synonym: String,
    score: f32,
    semantic: f32,
    lexical: usize,
}

/// Подобрать опорные сведения для написания BSL и проверить имеющийся черновик.
pub fn assist_bsl(conn: &Connection, args: &Value) -> Result<Value, String> {
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("нужен параметр task — что должен сделать код")?;
    let source = args.get("source").and_then(Value::as_str).unwrap_or("");
    let module_hint = args
        .get("module")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_LIMIT))
        .unwrap_or(DEFAULT_LIMIT);

    let terms = significant_terms(task);
    let use_semantic = args
        .get("semantic")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (semantic_methods, semantic_objects, semantic_warning) = if use_semantic {
        semantic_candidates(conn, task, limit)
    } else {
        (HashMap::new(), HashMap::new(), None)
    };
    let methods = method_candidates(conn, &terms, module_hint, &semantic_methods, limit)?;
    let objects = object_candidates(conn, &terms, &semantic_objects, limit)?;
    let completions = completions(conn, source, args.get("cursor"), limit)?;
    let platform = if args
        .get("platform_help")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        crate::platform_help::suggest(task, source, args.get("cursor"), limit)
    } else {
        json!({"available": false, "items": [], "completions": [], "reason": "отключено параметром platform_help"})
    };
    let platform_target = platform["target_owners"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    let examples = if platform_target {
        Vec::new()
    } else {
        examples(conn, &methods, limit.min(3))?
    };

    let method_rows: Vec<Value> = methods
        .iter()
        .filter(|_| !platform_target)
        .map(|m| {
            let completion = callable_name(m, module_hint).map(|name| format!("{name}("));
            json!({
                "name": m.name,
                "kind": m.kind,
                "params": m.params,
                "is_export": m.export,
                "module": m.path,
                "object": m.object,
                "module_type": m.module_type,
                "line": m.line,
                "score": rounded(m.score),
                "signals": {"semantic": rounded(m.semantic), "lexical": m.lexical},
                "completion": completion,
                "use": if completion.is_some() { "callable" } else { "reference_only" },
            })
        })
        .collect();

    let mut object_rows = Vec::new();
    for o in objects.iter().filter(|_| !platform_target) {
        let attrs = attributes(conn, &o.name)?;
        object_rows.push(json!({
            "name": o.name,
            "category": o.category,
            "synonym": o.synonym,
            "score": rounded(o.score),
            "signals": {"semantic": rounded(o.semantic), "lexical": o.lexical},
            "attributes": attrs,
        }));
    }

    let diagnostics = if source.trim().is_empty() {
        Value::Null
    } else {
        crate::check_bsl::check_bsl(conn, &json!({"source": source, "limit": 20}))?
    };

    let callable = (!platform_target)
        .then(|| methods.iter().find_map(|m| callable_name(m, module_hint)))
        .flatten();
    let scaffold = if source.trim().is_empty() {
        Some(scaffold(task, callable.as_deref()))
    } else {
        None
    };

    let mut out = json!({
        "task": task,
        "module": module_hint,
        "scaffold": scaffold,
        "methods": method_rows,
        "objects": object_rows,
        "completions": completions,
        "examples": examples,
        "platform_api": platform,
        "diagnostics": diagnostics,
        "note": "Подсказки взяты из индекса конфигурации. callable означает допустимую форму имени, а не доказательство применимости метода к бизнес-задаче. reference_only — найденный пример, который нельзя безопасно предложить как внешний вызов. После правки вызовите assist_bsl повторно или check_bsl. Текст запросов 1С здесь не проверяется."
    });
    if let Some(w) = semantic_warning {
        out["semantic_warning"] = json!(w);
    }
    Ok(out)
}

fn semantic_candidates(
    conn: &Connection,
    task: &str,
    limit: usize,
) -> (HashMap<i64, f32>, HashMap<i64, f32>, Option<String>) {
    let mut methods = HashMap::new();
    let mut objects = HashMap::new();
    match gyrfalcon_index::semantic::search(conn, task, None, (limit * 8).clamp(20, 100)) {
        Ok(hits) => {
            for h in hits {
                match h.kind.as_str() {
                    "method" => {
                        methods.insert(h.ref_id, h.raw);
                    }
                    "object" => {
                        objects.insert(h.ref_id, h.raw);
                    }
                    _ => {}
                }
            }
            (methods, objects, None)
        }
        Err(e) => (
            methods,
            objects,
            Some(format!(
                "семантический поиск недоступен ({e}); использован лексический резерв"
            )),
        ),
    }
}

fn method_candidates(
    conn: &Connection,
    terms: &[String],
    module_hint: &str,
    semantic: &HashMap<i64, f32>,
    limit: usize,
) -> Result<Vec<MethodCandidate>, String> {
    let mut ids: HashSet<i64> = semantic.keys().copied().collect();
    ids.extend(fts_ids(conn, "methods_fts", terms, limit * 30)?);
    // Текущий модуль — точный контекст, а не поисковый сигнал. Его локальные
    // методы полезны даже когда слова задачи не встречаются в имени.
    if !module_hint.is_empty() {
        let pattern = format!("%{module_hint}%");
        let mut st = conn
            .prepare(
                "SELECT m.id FROM methods m JOIN modules mo ON mo.id=m.module_id
                 WHERE mo.rel_path LIKE ?1 COLLATE NOCASE
                    OR mo.object_name = ?2 COLLATE NOCASE LIMIT 100",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map(rusqlite::params![pattern, module_hint], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        for id in rows {
            ids.insert(id.map_err(|e| e.to_string())?);
        }
    }

    let mut st = conn
        .prepare(
            "SELECT m.id, m.name, m.type, COALESCE(m.params,''), m.is_export,
                    m.line, mo.rel_path, COALESCE(mo.object_name,''),
                    COALESCE(mo.module_type,''), COALESCE(mo.category,'')
             FROM methods m JOIN modules mo ON mo.id = m.module_id
             WHERE m.id = ?1",
        )
        .map_err(|e| e.to_string())?;
    let hint = module_hint.to_lowercase();
    let mut out = Vec::new();
    for id in ids {
        let row = st.query_row([id], |r| {
            Ok(MethodCandidate {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                params: r.get(3)?,
                export: r.get::<_, i64>(4).unwrap_or(0) == 1,
                line: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                path: r.get(6)?,
                object: r.get(7)?,
                module_type: r.get(8)?,
                category: r.get(9)?,
                score: 0.0,
                semantic: 0.0,
                lexical: 0,
            })
        });
        let Ok(mut m) = row else {
            continue;
        };
        m.semantic = semantic.get(&m.id).copied().unwrap_or(0.0);
        m.lexical = lexical_weight(&format!("{} {}", m.name, m.object), terms);
        let context = if !hint.is_empty()
            && (m.path.to_lowercase().contains(&hint) || m.object.to_lowercase() == hint)
        {
            20.0
        } else {
            0.0
        };
        m.score = m.semantic * 100.0 + m.lexical as f32 + context;
        let externally_callable =
            m.export && m.category.eq_ignore_ascii_case("CommonModules") && !m.object.is_empty();
        // Без текущего модуля внутренний метод — не подсказка для написания,
        // а случайный фрагмент чужой реализации. Он остаётся доступен через
        // find/read, но в помощник не проходит.
        if module_hint.is_empty() && !externally_callable {
            continue;
        }
        if m.semantic >= MIN_SEMANTIC_RAW || m.lexical >= MIN_LEXICAL || context > 0.0 {
            out.push(m);
        }
    }
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.export.cmp(&a.export))
            .then_with(|| a.name.cmp(&b.name))
    });
    out.truncate(limit);
    Ok(out)
}

fn object_candidates(
    conn: &Connection,
    terms: &[String],
    semantic: &HashMap<i64, f32>,
    limit: usize,
) -> Result<Vec<ObjectCandidate>, String> {
    let mut ids: HashSet<i64> = semantic.keys().copied().collect();
    ids.extend(fts_ids(conn, "objects_fts", terms, limit * 20)?);
    let mut st = conn
        .prepare(
            "SELECT id, object_name, category, COALESCE(synonym,'')
             FROM object_synonyms WHERE id=?1",
        )
        .map_err(|e| e.to_string())?;
    let mut by_name: HashMap<String, ObjectCandidate> = HashMap::new();
    for id in ids {
        let row = st.query_row([id], |r| {
            Ok(ObjectCandidate {
                id: r.get(0)?,
                name: r.get(1)?,
                category: r.get(2)?,
                synonym: r.get(3)?,
                score: 0.0,
                semantic: 0.0,
                lexical: 0,
            })
        });
        let Ok(mut o) = row else {
            continue;
        };
        o.semantic = semantic.get(&o.id).copied().unwrap_or(0.0);
        let searchable = format!("{} {}", o.name, o.synonym);
        o.lexical = lexical_weight(&searchable, terms);
        o.score = o.semantic * 100.0 + o.lexical as f32;
        let matches = match_count(&searchable, terms);
        // В задаче из нескольких слов объект, совпавший только по одному
        // общему слову («структура», «добавить»), почти всегда шум.
        if o.semantic < MIN_SEMANTIC_RAW
            && (o.lexical < MIN_LEXICAL || (terms.len() > 1 && matches < 2))
        {
            continue;
        }
        let key = o.name.to_lowercase();
        match by_name.get(&key) {
            Some(old) if old.score >= o.score => {}
            _ => {
                by_name.insert(key, o);
            }
        }
    }
    let mut out: Vec<_> = by_name.into_values().collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.name.cmp(&b.name))
    });
    out.truncate(limit.min(5));
    Ok(out)
}

/// Взять только адреса кандидатов через триграммный FTS. Полного прохода по
/// сотням тысяч методов здесь быть не должно: первый живой прогон на 564 702
/// методах не уложился в 30 секунд именно из-за такого прохода.
fn fts_ids(
    conn: &Connection,
    table: &str,
    terms: &[String],
    limit: usize,
) -> Result<HashSet<i64>, String> {
    if table != "methods_fts" && table != "objects_fts" {
        return Err(format!("неизвестная FTS-таблица: {table}"));
    }
    let sql = format!("SELECT rowid FROM {table} WHERE {table} MATCH ?1 LIMIT ?2");
    let mut st = conn
        .prepare(&sql)
        .map_err(|e| format!("в индексе нет {table}; пересоберите индекс новой версией: {e}"))?;
    let mut out = HashSet::new();
    for term in terms {
        let rows = st
            .query_map(rusqlite::params![term, limit as i64], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        for id in rows {
            out.insert(id.map_err(|e| e.to_string())?);
        }
    }
    Ok(out)
}

fn attributes(conn: &Connection, object: &str) -> Result<Value, String> {
    let mut st = conn
        .prepare(
            "SELECT attr_name, attr_type, attr_kind, COALESCE(ts_name,'')
             FROM object_attributes
             WHERE object_name = ?1 COLLATE NOCASE
             ORDER BY attr_kind, ts_name, attr_name LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map(rusqlite::params![object, ATTR_LIMIT], |r| {
            Ok(json!({
                "name": r.get::<_, String>(0)?,
                "type": r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                "kind": r.get::<_, String>(2)?,
                "table_section": r.get::<_, String>(3)?,
            }))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| e.to_string())?);
    }
    let shown = out.len();
    Ok(json!({"rows": out, "shown": shown, "limit": ATTR_LIMIT}))
}

fn completions(
    conn: &Connection,
    source: &str,
    cursor: Option<&Value>,
    limit: usize,
) -> Result<Vec<Value>, String> {
    if source.is_empty() {
        return Ok(Vec::new());
    }
    let total = source.chars().count();
    let at = cursor
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(total)
        .min(total);
    let before: String = source.chars().take(at).collect();
    let prefix: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if prefix.chars().count() < 2 {
        return Ok(Vec::new());
    }

    let (qualifier, name_prefix) = prefix.rsplit_once('.').unwrap_or(("", prefix.as_str()));
    let pattern = format!("{name_prefix}%");
    let mut out = Vec::new();
    if qualifier.is_empty() {
        let mut st = conn
            .prepare(
                "SELECT DISTINCT m.name, COALESCE(m.params,''), mo.rel_path
                 FROM methods m JOIN modules mo ON mo.id=m.module_id
                 WHERE m.name LIKE ?1 COLLATE NOCASE
                 ORDER BY m.name LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map(rusqlite::params![pattern, limit as i64], |r| {
                Ok(json!({
                    "replace": prefix,
                    "with": r.get::<_, String>(0)?,
                    "params": r.get::<_, String>(1)?,
                    "module": r.get::<_, String>(2)?,
                }))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
    } else {
        let mut st = conn
            .prepare(
                "SELECT DISTINCT m.name, COALESCE(m.params,''), mo.rel_path
                 FROM methods m JOIN modules mo ON mo.id=m.module_id
                 WHERE mo.object_name = ?1 COLLATE NOCASE
                   AND m.name LIKE ?2 COLLATE NOCASE AND m.is_export = 1
                 ORDER BY m.name LIMIT ?3",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map(rusqlite::params![qualifier, pattern, limit as i64], |r| {
                let name: String = r.get(0)?;
                Ok(json!({
                    "replace": prefix,
                    "with": format!("{qualifier}.{name}"),
                    "params": r.get::<_, String>(1)?,
                    "module": r.get::<_, String>(2)?,
                }))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
    }
    Ok(out)
}

fn examples(
    conn: &Connection,
    methods: &[MethodCandidate],
    limit: usize,
) -> Result<Vec<Value>, String> {
    let root: Option<String> = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='source_path'",
            [],
            |r| r.get(0),
        )
        .ok();
    let Some(root) = root else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for m in methods {
        if out.len() >= limit {
            break;
        }
        let key = format!("{}::{}", m.path, m.name.to_lowercase());
        let mut st = conn
            .prepare(
                "SELECT mo.rel_path, caller.name, c.line, c.callee_name
                 FROM calls c
                 JOIN methods caller ON caller.id=c.caller_id
                 JOIN modules mo ON mo.id=caller.module_id
                 WHERE c.callee_key = ?1
                 ORDER BY c.confidence DESC, c.line LIMIT 2",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([&key], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (path, caller, line, call) = row.map_err(|e| e.to_string())?;
            if line > 0 {
                if let Some((line_start, snippet)) = source_fragment(Path::new(&root), &path, line)
                {
                    out.push(json!({
                        "call": call,
                        "caller": caller,
                        "module": path,
                        "line": line,
                        "snippet_line": line_start,
                        "snippet": snippet,
                    }));
                }
            }
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

fn source_fragment(root: &Path, rel_path: &str, line: i64) -> Option<(usize, String)> {
    let bytes = std::fs::read(root.join(rel_path)).ok()?;
    let text = String::from_utf8_lossy(
        bytes
            .strip_prefix(&[0xEF, 0xBB, 0xBF])
            .unwrap_or(bytes.as_slice()),
    );
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let center = line.max(1) as usize - 1;
    let start = center.saturating_sub(1).min(lines.len());
    let end = (center + 2).min(lines.len());
    let snippet: String = lines[start..end]
        .join("\n")
        .chars()
        .take(EXAMPLE_CHARS)
        .collect();
    Some((start + 1, snippet))
}

fn callable_name(m: &MethodCandidate, module_hint: &str) -> Option<String> {
    let hint = module_hint.to_lowercase();
    if !hint.is_empty()
        && (m.path.to_lowercase().contains(&hint) || m.object.to_lowercase() == hint)
    {
        return Some(m.name.clone());
    }
    if m.export && m.category.eq_ignore_ascii_case("CommonModules") && !m.object.is_empty() {
        return Some(format!("{}.{}", m.object, m.name));
    }
    None
}

fn scaffold(task: &str, callable: Option<&str>) -> String {
    let name = procedure_name(task);
    let mut lines = vec![format!("Процедура {name}()"), format!("\t// {task}")];
    if let Some(call) = callable {
        lines.push(format!("\t// Подходящий вызов из индекса: {call}(...)"));
    }
    lines.push("КонецПроцедуры".to_string());
    lines.join("\n")
}

fn procedure_name(task: &str) -> String {
    let mut result = String::new();
    for word in task
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .take(4)
    {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            result.extend(first.to_uppercase());
            result.extend(chars);
        }
    }
    if result.is_empty() {
        result.push_str("ВыполнитьЗадачу");
    }
    if result.chars().next().is_some_and(|c| c.is_numeric()) {
        result.insert_str(0, "Выполнить");
    }
    result
}

fn significant_terms(text: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "для",
        "как",
        "при",
        "или",
        "это",
        "что",
        "код",
        "нужно",
        "надо",
        "через",
        "the",
        "and",
        "with",
        "from",
    ];
    let mut out = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let word = word.to_lowercase();
        if word.chars().count() >= 3 && !STOP.contains(&word.as_str()) && !out.contains(&word) {
            out.push(word);
        }
    }
    out.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));
    out.truncate(8);
    out
}

fn lexical_weight(text: &str, terms: &[String]) -> usize {
    let hay = text.to_lowercase();
    terms
        .iter()
        .filter(|term| hay.contains(term.as_str()))
        .map(|term| term.chars().count())
        .sum()
}

fn match_count(text: &str, terms: &[String]) -> usize {
    let hay = text.to_lowercase();
    terms
        .iter()
        .filter(|term| hay.contains(term.as_str()))
        .count()
}

fn rounded(v: f32) -> f64 {
    ((v as f64) * 1000.0).round() / 1000.0
}

/// Схема параметров для `tools/list`.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task": {
                "type": "string",
                "description": "Что должен сделать код — предметная задача на естественном языке"
            },
            "module": {
                "type": "string",
                "description": "Текущий модуль: путь из find/read или имя объекта. Повышает релевантность локальных методов"
            },
            "source": {
                "type": "string",
                "default": "",
                "description": "Текущий черновик BSL. Пусто — вернуть каркас; непусто — дополнения и check_bsl"
            },
            "cursor": {
                "type": "integer",
                "description": "Позиция курсора в source, в символах Unicode; по умолчанию конец текста"
            },
            "limit": {
                "type": "integer",
                "default": DEFAULT_LIMIT,
                "description": "Сколько методов и дополнений показать, 1..20"
            },
            "semantic": {
                "type": "boolean",
                "default": false,
                "description": "Добавить полный поиск по смыслу. Он дороже; включать, когда строгий лексический поиск ничего не дал"
            },
            "platform_help": {
                "type": "boolean",
                "default": true,
                "description": "Подмешать точный синтаксис встроенных типов и методов из локального shcntx_ru.hbk"
            },
            "project": {
                "oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}],
                "description": "Конфигурация 1С. Обязательна при нескольких проектах (см. list_projects)"
            }
        },
        "required": ["task"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE modules(
                id INTEGER PRIMARY KEY, rel_path TEXT, category TEXT,
                object_name TEXT, module_type TEXT
             );
             CREATE TABLE methods(
                id INTEGER PRIMARY KEY, module_id INTEGER, name TEXT, type TEXT,
                params TEXT, is_export INTEGER, line INTEGER
             );
             CREATE TABLE calls(
                id INTEGER PRIMARY KEY, caller_id INTEGER, callee_name TEXT,
                line INTEGER, callee_key TEXT, confidence REAL
             );
             CREATE TABLE object_synonyms(
                id INTEGER PRIMARY KEY, object_name TEXT, category TEXT, synonym TEXT
             );
             CREATE TABLE object_attributes(
                id INTEGER PRIMARY KEY, object_name TEXT, attr_name TEXT,
                attr_type TEXT, attr_kind TEXT, ts_name TEXT
             );
             CREATE VIRTUAL TABLE methods_fts USING fts5(name, tokenize='trigram');
             CREATE VIRTUAL TABLE objects_fts USING fts5(name, tokenize='trigram');
             CREATE TABLE predefined_items(item_name TEXT);
             CREATE TABLE index_meta(key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO modules VALUES
                (1, 'CommonModules/Заказы/Ext/Module.bsl', 'CommonModules', 'Заказы', 'Module'),
                (2, 'Documents/ЗаказКлиента/Ext/ObjectModule.bsl', 'Documents', 'ЗаказКлиента', 'ObjectModule');
             INSERT INTO methods VALUES
                (1, 1, 'ЗаполнитьЗаказ', 'Процедура', '(Заказ, Данные)', 1, 10),
                (2, 1, 'СоздатьЗаказ', 'Функция', '(Контрагент)', 1, 30),
                (3, 2, 'ПередЗаписью', 'Процедура', '(Отказ)', 0, 5);
             INSERT INTO object_synonyms VALUES
                (1, 'ЗаказКлиента', 'Documents', 'Заказ клиента');
             INSERT INTO methods_fts(rowid,name)
                SELECT id,name FROM methods;
             INSERT INTO objects_fts(rowid,name)
                SELECT id,object_name || ' ' || synonym FROM object_synonyms;
             INSERT INTO object_attributes VALUES
                (1, 'ЗаказКлиента', 'Контрагент', 'CatalogRef.Контрагенты', 'attribute', '');",
        )
        .unwrap();
        c
    }

    #[test]
    fn возвращает_каркас_метод_и_объект() {
        let v = assist_bsl(
            &index(),
            &json!({"task": "заполнить заказ клиента", "limit": 5, "platform_help": false, "semantic": true}),
        )
        .unwrap();
        assert!(v["scaffold"].as_str().unwrap().contains("Процедура"));
        assert_eq!(v["methods"][0]["name"], json!("ЗаполнитьЗаказ"));
        assert_eq!(
            v["methods"][0]["completion"],
            json!("Заказы.ЗаполнитьЗаказ(")
        );
        assert_eq!(v["objects"][0]["name"], json!("ЗаказКлиента"));
        assert_eq!(v["objects"][0]["attributes"]["shown"], json!(1));
        assert!(v.get("semantic_warning").is_some());
    }

    #[test]
    fn дополняет_квалифицированный_вызов_и_проверяет_черновик() {
        let source = "Процедура Т()\n\tЗаказы.Зап\nКонецПроцедуры";
        let cursor = source[..source.find("\nКонец").unwrap()].chars().count();
        let v = assist_bsl(
            &index(),
            &json!({
                "task": "заполнить заказ",
                "source": source,
                "cursor": cursor,
                "platform_help": false
            }),
        )
        .unwrap();
        assert_eq!(v["scaffold"], Value::Null);
        assert_eq!(v["completions"][0]["with"], json!("Заказы.ЗаполнитьЗаказ"));
        assert!(v["diagnostics"].is_object());
    }

    #[test]
    fn task_обязателен() {
        let err = assist_bsl(&index(), &json!({})).unwrap_err();
        assert!(err.contains("task"));
    }
}
