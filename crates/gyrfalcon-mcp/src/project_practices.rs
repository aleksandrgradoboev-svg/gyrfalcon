//! Общий слой стандартов 1С и проверенный профиль практик выбранного проекта.
//!
//! Стандарты хранятся один раз в бинарном каталоге. Проектный профиль лежит
//! рядом с исходниками в `.gyrfalcon/project-practices.json` и содержит только
//! локальные кандидаты/решения; наблюдения требуют проверки по индексу.

use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

fn write_profile_atomic(
    path: &std::path::Path,
    contents: &str,
    create_only: bool,
) -> Result<(), String> {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let temp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), suffix));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| format!("не создан временный профиль: {e}"))?;
        file.write_all(contents.as_bytes())
            .map_err(|e| format!("не записан временный профиль: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("не синхронизирован временный профиль: {e}"))?;
        drop(file);
        if create_only {
            match fs::hard_link(&temp, path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
                Err(e) => Err(format!("не опубликован новый профиль: {e}")),
            }
        } else {
            fs::rename(&temp, path).map_err(|e| format!("не заменён профиль: {e}"))
        }
    })();
    let _ = fs::remove_file(&temp);
    result
}

fn require_fresh_index(conn: &Connection, source: &str, commit: &str) -> Result<(), String> {
    let built_at: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='built_at'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| "для поиска практик нужен индекс с built_at".to_string())?;
    let built_at: u64 = built_at
        .parse()
        .map_err(|_| "некорректный built_at индекса")?;
    let freshness = gyrfalcon_index::freshness::check(built_at, source, commit);
    if let Some(ref reason) = freshness.unchecked {
        return Err(format!("свежесть индекса не проверена: {reason}"));
    }
    if freshness.stale {
        return Err(freshness.note().unwrap_or_else(|| "индекс устарел".into()));
    }
    Ok(())
}

const MIN_SAMPLE: i64 = 10;
const MIN_PRECISION: f64 = 0.90;

pub fn profile(conn: &Connection, args: &Value) -> Result<Value, String> {
    let project = args
        .get("project")
        .and_then(Value::as_str)
        .filter(|p| !p.trim().is_empty() && *p != "*")
        .ok_or("project_practices требует одно явное имя project из list_projects")?;
    let source: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='source_path'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("не прочитан source_path индекса: {e}"))?;
    let commit: Option<String> = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='git_commit'",
            [],
            |r| r.get(0),
        )
        .ok();
    let path = PathBuf::from(&source)
        .join(".gyrfalcon")
        .join("project-practices.json");
    let auto_created = !path.is_file();
    if auto_created {
        let Some(commit) = commit.as_deref() else {
            return empty(conn, project, &source, None, "index_commit_missing");
        };
        create_discovery_if_missing(conn, &path, project, commit)?;
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("не прочитан профиль практик {}: {e}", path.display()))?;
    let mut doc: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("некорректный JSON профиля практик: {e}"))?;
    if doc.get("version").and_then(Value::as_i64) != Some(1) {
        return Err("profile practices: поддерживается только version=1".into());
    }
    if doc.get("project").and_then(Value::as_str) != Some(project) {
        return Err("profile practices: project не совпадает с выбранным индексом".into());
    }
    let mut auto_populated = false;
    if !auto_created
        && doc["rules"].as_array().is_some_and(Vec::is_empty)
        && doc["discovery"]["completed"] != true
        && doc["source_commit"].as_str() == commit.as_deref()
    {
        let commit = commit
            .as_deref()
            .ok_or("пустой профиль: у индекса нет git_commit")?;
        require_fresh_index(conn, &source, commit)?;
        let mined = crate::practice_mining::mine(
            conn,
            &source,
            crate::practice_mining::MiningDepth::Quick,
        )?;
        require_fresh_index(conn, &source, commit)?;
        doc["rules"] = json!(mined.candidates);
        doc["discovery"] = mined.coverage;
        let encoded = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        write_profile_atomic(&path, &format!("{encoded}\n"), false)?;
        auto_populated = true;
    }
    if commit.as_deref().is_none()
        || doc.get("source_commit").and_then(Value::as_str) != commit.as_deref()
    {
        // Наблюдения из кода устаревают вместе с индексом. Явно принятые
        // требования заказчика от коммита не зависят и должны остаться в силе.
        let normative = normative_rules(&doc)?;
        let mut answer = empty(conn, project, &source, commit.as_deref(), "commit_mismatch")?;
        let mut combined = standard_baseline()?;
        combined.extend(normative.iter().cloned());
        let (effective, suppressed) = resolve_conflicts(combined);
        answer["rules"] = json!(effective);
        answer["suppressed"] = json!(suppressed);
        answer["metrics"]["accepted"] = json!(normative.len());
        answer["metrics"]["effective"] = json!(answer["rules"].as_array().map_or(0, Vec::len));
        answer["note"] = json!("Индекс/профиль разошлись по коммиту: наблюдения из кода скрыты, явно принятые проектные требования сохраняют силу.");
        return Ok(answer);
    }
    let candidates = doc
        .get("rules")
        .and_then(Value::as_array)
        .ok_or("profile practices: нужен массив rules")?;
    let mut rules = Vec::new();
    let mut rejected = 0usize;
    let mut pending = 0usize;
    for rule in candidates {
        if rule.get("status").and_then(Value::as_str) == Some("candidate") {
            pending += 1;
            continue;
        }
        match validate(conn, rule) {
            Ok(rule) => rules.push(rule),
            Err(_) => rejected += 1,
        }
    }
    let accepted = rules.len();
    let baseline = standard_baseline()?;
    let baseline_count = baseline.len();
    let corpus = crate::standards_1c::stats(conn);
    let mut combined = baseline.clone();
    combined.extend(rules);
    let (rules, suppressed) = resolve_conflicts(combined);
    let effective = rules.len();
    let mut answer = json!({
        "source": "shared 1C standards + verified project practices", "read_only": !auto_created,
        "profile_auto_created": auto_created,
        "profile_auto_populated": auto_populated,
        "project": project, "source_path": source, "source_commit": commit,
        "profile_version": 1, "baseline": {"kind":"shared-1c-standards","version":1,
            "rules":baseline_count,"project_copy":false,"corpus":corpus,
            "search_tool":"standards_1c"},
        "rules": rules, "suppressed": suppressed,
        "discovery": doc.get("discovery").cloned().unwrap_or(Value::Null),
        "metrics": {"candidates": candidates.len(), "accepted": accepted,
                    "baseline":baseline_count,"effective": effective,
                    "pending": pending, "rejected": rejected,
                    "precision_threshold": MIN_PRECISION, "minimum_sample": MIN_SAMPLE},
        "note": "В rules общий стандартный слой и принятые проектные правила с учётом приоритета; локальные кандидаты доступны при view=candidates."
    });
    match args.get("view").and_then(Value::as_str) {
        None | Some("verified") => {}
        Some("candidates") => {
            answer["candidates"] = json!(candidates
                .iter()
                .filter(|r| r.get("status").and_then(Value::as_str) != Some("verified"))
                .collect::<Vec<_>>());
            answer["candidate_conflicts"] = json!(candidates
                .iter()
                .filter(|r| r["status"] == "candidate")
                .flat_map(|r| baseline
                    .iter()
                    .filter(move |b| r["id"] != b["id"] && names_conflict(r, b))
                    .map(move |b| json!({"candidate":r["id"],"standard":b["id"],
                        "status":"pending_acceptance"})))
                .collect::<Vec<_>>());
            answer["candidate_covered_by_baseline"] = json!(candidates
                .iter()
                .filter(
                    |r| r["status"] == "candidate" && baseline.iter().any(|b| b["id"] == r["id"])
                )
                .map(|r| r["id"].clone())
                .collect::<Vec<_>>());
        }
        Some(_) => return Err("view: допустимо verified или candidates".into()),
    }
    Ok(answer)
}

fn authority_rank(rule: &Value) -> u8 {
    match rule.get("authority").and_then(Value::as_str) {
        Some("customer") => 4,
        Some("project") => 3,
        Some("standard") => 2,
        _ => 1,
    }
}

fn names_conflict(left: &Value, right: &Value) -> bool {
    let Some(left_id) = left.get("id").and_then(Value::as_str) else {
        return false;
    };
    let Some(right_id) = right.get("id").and_then(Value::as_str) else {
        return false;
    };
    if left_id == right_id {
        return true;
    }
    left.get("conflicts_with")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(right_id)))
        || right
            .get("conflicts_with")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(left_id)))
}

fn resolve_conflicts(rules: Vec<Value>) -> (Vec<Value>, Vec<Value>) {
    let mut active = Vec::new();
    let mut suppressed = Vec::new();
    for rule in &rules {
        let higher = rules.iter().find(|other| {
            names_conflict(rule, other) && authority_rank(other) > authority_rank(rule)
        });
        if let Some(other) = higher {
            suppressed.push(json!({"rule":rule["id"],"by":other["id"],
                "reason":"higher_authority_conflict"}));
        } else {
            active.push(rule.clone());
        }
    }
    (active, suppressed)
}

fn create_discovery_if_missing(
    conn: &Connection,
    path: &std::path::Path,
    project: &str,
    commit: &str,
) -> Result<(), String> {
    let source: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='source_path'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("не прочитан source_path индекса: {e}"))?;
    require_fresh_index(conn, &source, commit)?;
    let mined =
        crate::practice_mining::mine(conn, &source, crate::practice_mining::MiningDepth::Quick)?;
    require_fresh_index(conn, &source, commit)?;
    let parent = path.parent().ok_or("некорректный путь профиля")?;
    fs::create_dir_all(parent).map_err(|e| format!("не создан каталог профиля: {e}"))?;
    let doc = json!({"version":1,"project":project,"source_commit":commit,
        "discovery":mined.coverage,"rules":mined.candidates});
    let encoded = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    write_profile_atomic(path, &format!("{encoded}\n"), true)
}

/// Явная команда добавляет найденные семейства, не меняя имеющиеся правила.
pub fn discover(conn: &Connection, project: &str) -> Result<usize, String> {
    discover_with_depth(conn, project, crate::practice_mining::MiningDepth::Quick)
}

pub fn discover_with_depth(
    conn: &Connection,
    project: &str,
    depth: crate::practice_mining::MiningDepth,
) -> Result<usize, String> {
    let source: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='source_path'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("не прочитан source_path индекса: {e}"))?;
    let commit: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='git_commit'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| "построение требует индекс с git_commit".to_string())?;
    require_fresh_index(conn, &source, &commit)?;
    let mut rules = load_rules(&source, project, &commit)?;
    let before = rules.len();
    let mined = crate::practice_mining::mine(conn, &source, depth)?;
    require_fresh_index(conn, &source, &commit)?;
    for candidate in mined.candidates {
        let id = candidate["id"].as_str();
        if !rules.iter().any(|r| r["id"].as_str() == id) {
            rules.push(candidate);
        }
    }
    let added = rules.len() - before;
    if added > 0
        || !PathBuf::from(&source)
            .join(".gyrfalcon/project-practices.json")
            .is_file()
    {
        save_rules_with_discovery(&source, project, &commit, rules, Some(mined.coverage))?;
    }
    Ok(added)
}

/// Единый встроенный слой стандартов. Ничего не записывает в проектный профиль.
pub(crate) fn standard_baseline() -> Result<Vec<Value>, String> {
    let catalog: Value = serde_json::from_str(include_str!("one_c_standards.json"))
        .map_err(|e| format!("некорректный каталог стандартов 1С: {e}"))?;
    if catalog.get("version").and_then(Value::as_i64) != Some(1) {
        return Err("каталог стандартов 1С: нужна version=1".into());
    }
    let entries = catalog["rules"]
        .as_array()
        .ok_or("каталог стандартов 1С: нужен массив rules")?;
    let mut rules = Vec::with_capacity(entries.len());
    let mut ids = HashSet::new();
    for entry in entries {
        let id = entry["id"].as_str().ok_or("стандарт без id")?;
        if !ids.insert(id) {
            return Err(format!("повтор стандарта 1С: {id}"));
        }
        let url = entry["url"].as_str().ok_or("стандарт без url")?;
        if !url.starts_with("https://its.1c.ru/db/content/v8std/") {
            return Err(format!("стандарт {id}: ссылка не относится к 1С:ИТС"));
        }
        if entry.get("accepted_batch").is_some() {
            let approved = entry.get("approval").is_some_and(|approval| {
                let explicitly_accepted =
                    approval["kind"] == "explicit" && approval["reviewer"] == "user";
                let delegated_acceptance =
                    approval["kind"] == "delegated" && approval["reviewer"] == "gyrfalcon";
                (explicitly_accepted || delegated_acceptance)
                    && approval["authority"] == "standard"
                    && approval["date"].as_str().is_some()
            });
            if !approved
                || entry["source_anchor"].as_str().is_none()
                || entry["source_sha256"]
                    .as_str()
                    .is_none_or(|sha| sha.len() != 64)
            {
                return Err(format!(
                    "стандарт {id}: нет подтверждённой приёмки источника"
                ));
            }
        }
        let reference = json!({"kind":"1c-standard","url":url,
            "section":entry["section"],
            "scope":entry.get("scope").and_then(Value::as_str).unwrap_or("bsl-modules"),
            "anchor":entry.get("source_anchor"),
            "sha256":entry.get("source_sha256"),
            "related_sources":entry.get("related_sources")});
        rules.push(json!({"id":id,"title":entry["title"],
            "guidance":entry["guidance"],"authority":"standard",
            "source":reference,
            "exception":entry.get("exception"),
            "verification":entry.get("verification"),
            "approval":entry.get("approval"),
            "conflicts_with":entry.get("conflicts_with").cloned().unwrap_or(json!([]))}));
    }
    Ok(rules)
}

fn empty(
    conn: &Connection,
    project: &str,
    source: &str,
    commit: Option<&str>,
    status: &str,
) -> Result<Value, String> {
    let baseline = standard_baseline()?;
    let count = baseline.len();
    let corpus = crate::standards_1c::stats(conn);
    Ok(
        json!({"source":"shared 1C standards + verified project practices",
        "read_only":true,"project":project,"source_path":source,"source_commit":commit,
        "profile_version":1,"baseline":{"kind":"shared-1c-standards","version":1,
        "rules":count,"project_copy":false,"corpus":corpus,
        "search_tool":"standards_1c"},"rules":baseline,
        "metrics":{"candidates":0,"accepted":0,"baseline":count,"effective":count,
        "pending":0,"rejected":0,"precision_threshold":MIN_PRECISION,"minimum_sample":MIN_SAMPLE},
        "status":status,"note":"Проектный профиль недоступен; общий слой стандартов 1С остаётся доступен."}),
    )
}

fn validate(conn: &Connection, r: &Value) -> Result<Value, ()> {
    if r.get("status").and_then(Value::as_str) != Some("verified")
        || r.get("legacy").and_then(Value::as_bool) != Some(false)
    {
        return Err(());
    }
    if let Some(approval) = r.get("approval") {
        if approval.get("kind").and_then(Value::as_str) != Some("explicit") {
            return Err(());
        }
        let authority = approval
            .get("authority")
            .and_then(Value::as_str)
            .ok_or(())?;
        let reviewer = approval.get("reviewer").and_then(Value::as_str).ok_or(())?;
        let reason = approval.get("reason").and_then(Value::as_str).ok_or(())?;
        if reviewer.trim().is_empty()
            || reason.trim().is_empty()
            || !normative_authority_allowed(r, authority)
        {
            return Err(());
        }
        for field in ["id", "title", "guidance"] {
            if r.get(field)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(());
            }
        }
        return Ok(
            json!({"id":r["id"],"title":r["title"],"guidance":r["guidance"],
            "authority":authority,"approval":approval,"source":r["source"],
            "references":r.get("references").cloned().unwrap_or(json!([])),
            "conflicts_with":r.get("conflicts_with").cloned().unwrap_or(json!([]))}),
        );
    }
    let sample = r.get("sample").and_then(Value::as_i64).ok_or(())?;
    let matches = r.get("matches").and_then(Value::as_i64).ok_or(())?;
    if sample < MIN_SAMPLE
        || matches < 0
        || matches > sample
        || (matches as f64 / sample as f64) < MIN_PRECISION
    {
        return Err(());
    }
    let evidence = r.get("evidence").and_then(Value::as_array).ok_or(())?;
    if evidence.is_empty() {
        return Err(());
    }
    for e in evidence {
        let module = e.get("module").and_then(Value::as_str).ok_or(())?;
        let method = e.get("method").and_then(Value::as_str).ok_or(())?;
        let present: i64 = conn.query_row(
            "SELECT COUNT(*) FROM methods m JOIN modules mo ON mo.id=m.module_id WHERE mo.rel_path=?1 AND m.name=?2",
            [module, method], |row| row.get(0),
        ).map_err(|_| ())?;
        if present == 0 {
            return Err(());
        }
    }
    let id = r
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(())?;
    let title = r
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(())?;
    let guidance = r
        .get("guidance")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(())?;
    Ok(
        json!({"id":id,"title":title,"guidance":guidance,"sample":sample,"matches":matches,
        "precision": matches as f64 / sample as f64,"evidence":evidence,
        "counterexamples":r.get("counterexamples").cloned().unwrap_or(json!([])),
        "exceptions":r.get("exceptions").cloned().unwrap_or(Value::Null),
        "scope":r.get("scope").cloned().unwrap_or(Value::Null),
        "observation":r.get("observation").cloned().unwrap_or(Value::Null),
        "acceptance_proof":r.get("acceptance_proof").cloned().unwrap_or(Value::Null),
        "authority":r.get("authority").cloned().unwrap_or(json!("observed")),
        "references":r.get("references").cloned().unwrap_or(json!([])),
        "conflicts_with":r.get("conflicts_with").cloned().unwrap_or(json!([])),
        "source":r.get("source").cloned().unwrap_or(Value::Null)}),
    )
}

fn normative_rules(doc: &Value) -> Result<Vec<Value>, String> {
    let rules = doc["rules"]
        .as_array()
        .ok_or("profile practices: нужен массив rules")?;
    let dummy = Connection::open_in_memory().map_err(|e| e.to_string())?;
    Ok(rules
        .iter()
        .filter(|r| r.get("approval").is_some())
        .filter_map(|r| validate(&dummy, r).ok())
        .collect())
}

/// Принятые нормативные исключения читаются без автозаполнения профиля.
/// Они продолжают действовать при смене коммита; наблюдения из кода — нет.
pub(crate) fn accepted_normative_rules(
    conn: &Connection,
    project: &str,
) -> Result<Vec<Value>, String> {
    let source: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='source_path'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("не прочитан source_path индекса: {e}"))?;
    let path = PathBuf::from(source).join(".gyrfalcon/project-practices.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let doc: Value = serde_json::from_str(
        &fs::read_to_string(&path)
            .map_err(|e| format!("не прочитан профиль {}: {e}", path.display()))?,
    )
    .map_err(|e| format!("некорректный JSON профиля практик: {e}"))?;
    if doc["project"].as_str() != Some(project) || doc["version"].as_i64() != Some(1) {
        return Err("профиль практик не соответствует выбранному проекту или version=1".into());
    }
    normative_rules(&doc)
}

pub(crate) fn suppressed_standards(overrides: &[Value]) -> Result<Vec<Value>, String> {
    let mut suppressed = Vec::new();
    for standard in standard_baseline()? {
        for override_rule in overrides {
            if names_conflict(&standard, override_rule)
                && authority_rank(override_rule) > authority_rank(&standard)
            {
                suppressed.push(json!({"standard_id":standard["id"],
                    "by":override_rule["id"], "standard_url":standard["source"]["url"],
                    "standard_section":standard["source"]["section"]}));
            }
        }
    }
    Ok(suppressed)
}

fn normative_authority_allowed(rule: &Value, authority: &str) -> bool {
    let kind = rule["source"]["kind"].as_str();
    match authority {
        "customer" | "project" => matches!(kind, Some("manual" | "documentation")),
        _ => false,
    }
}

/// Нормативная приёмка опирается на явное решение и источник, а не на частоту
/// встречаемости требования в коде. Для index-scan остаётся эмпирическая accept.
pub fn approve(
    conn: &Connection,
    project: &str,
    id: &str,
    authority: &str,
    reviewer: &str,
    reason: &str,
) -> Result<(), String> {
    if reviewer.trim().is_empty() || reason.trim().is_empty() {
        return Err("приёмка требует reviewer и reason".into());
    }
    let source: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='source_path'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("не прочитан source_path индекса: {e}"))?;
    let commit: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='git_commit'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| "приёмка требует индекс с git_commit".to_string())?;
    let mut rules = load_rules(&source, project, &commit)?;
    let rule = rules
        .iter_mut()
        .find(|r| r["id"] == id)
        .ok_or_else(|| format!("приёмка: кандидат {id} не найден"))?;
    if rule["legacy"] == true || !normative_authority_allowed(rule, authority) {
        return Err(format!(
            "приёмка {id}: источник не допускает полномочие {authority}"
        ));
    }
    rule["status"] = json!("verified");
    rule["authority"] = json!(authority);
    rule["approval"] = json!({"kind":"explicit","authority":authority,
        "reviewer":reviewer,"reason":reason});
    validate(conn, rule).map_err(|_| format!("приёмка {id}: правило не прошло проверку"))?;
    save_rules(&source, project, &commit, rules)
}

pub fn import_document(
    src: &str,
    project: &str,
    commit: &str,
    document: &str,
) -> Result<usize, String> {
    let text =
        fs::read_to_string(document).map_err(|e| format!("не прочитана документация: {e}"))?;
    let mut rules = load_rules(src, project, commit)?;
    let before = rules.len();
    for (n, line) in text.lines().enumerate() {
        let title = line.trim_start_matches('#').trim();
        if line.trim_start().starts_with('#') && !title.is_empty() {
            let id = slug(title, n + 1);
            if !rules
                .iter()
                .any(|r| r.get("id").and_then(Value::as_str) == Some(&id))
            {
                rules.push(json!({"id":id,"title":title,"guidance":title,"status":"candidate","legacy":false,
                    "sample":0,"matches":0,"evidence":[],"source":{"kind":"documentation","path":document,"anchor":format!("line:{}",n+1)}}));
            }
        }
    }
    let added = rules.len() - before;
    save_rules(src, project, commit, rules)?;
    Ok(added)
}

pub fn add_manual(src: &str, project: &str, commit: &str, rule: Value) -> Result<(), String> {
    let mut rules = load_rules(src, project, commit)?;
    let id = rule
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("ручному правилу нужен id")?;
    for field in ["title", "guidance"] {
        if rule
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(format!("ручному правилу нужен {field}"));
        }
    }
    if rules
        .iter()
        .any(|r| r.get("id").and_then(Value::as_str) == Some(id))
    {
        return Err(format!("правило {id} уже есть"));
    }
    let mut rule = rule;
    rule["source"] = json!({"kind":"manual"});
    rule["status"] = json!("candidate");
    rule["sample"] = json!(0);
    rule["matches"] = json!(0);
    rule["evidence"] = json!([]);
    if rule.get("legacy").and_then(Value::as_bool).is_none() {
        rule["legacy"] = json!(false);
    }
    rules.push(rule);
    save_rules(src, project, commit, rules)
}

pub fn accept(
    conn: &Connection,
    project: &str,
    id: &str,
    sample: i64,
    matches: i64,
    module: &str,
    method: &str,
) -> Result<(), String> {
    if sample < MIN_SAMPLE {
        return Err(format!(
            "приёмка {id}: выборка {sample}, минимум {MIN_SAMPLE}"
        ));
    }
    if matches < 0 || matches > sample || (matches as f64 / sample as f64) < MIN_PRECISION {
        return Err(format!(
            "приёмка {id}: точность должна быть не ниже {MIN_PRECISION:.2}"
        ));
    }
    let source: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='source_path'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("не прочитан source_path индекса: {e}"))?;
    let commit: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='git_commit'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| "приёмка требует индекс с git_commit".to_string())?;
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM methods m JOIN modules mo ON mo.id=m.module_id WHERE mo.rel_path=?1 AND m.name=?2",
        [module, method], |row| row.get(0),
    ).map_err(|e| e.to_string())?;
    if present == 0 {
        return Err(format!(
            "приёмка {id}: доказательство не найдено в индексе: {module}::{method}"
        ));
    }
    let mut rules = load_rules(&source, project, &commit)?;
    let rule = rules
        .iter_mut()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("приёмка: кандидат {id} не найден"))?;
    if rule.get("legacy").and_then(Value::as_bool) == Some(true) {
        return Err(format!("приёмка {id}: legacy не публикуется"));
    }
    if rule["source"]["kind"] == "indexed-region-order" {
        if rule["sample"].as_i64() != Some(sample)
            || rule["matches"].as_i64() != Some(matches)
            || rule["evidence"]
                .as_array()
                .is_none_or(|e| e.len() < MIN_SAMPLE as usize)
            || !rule["counterexamples"].is_array()
        {
            return Err(format!(
                "приёмка {id}: нужны рассчитанная выборка, 10 адресов и разбор обратного порядка"
            ));
        }
        if !rule["evidence"].as_array().is_some_and(|examples| {
            examples
                .iter()
                .any(|e| e["module"] == module && e["method"] == method)
        }) {
            return Err(format!(
                "приёмка {id}: контрольный адрес не входит в выборку карточки"
            ));
        }
    }
    rule["status"] = json!("verified");
    rule["legacy"] = json!(false);
    rule["sample"] = json!(sample);
    rule["matches"] = json!(matches);
    // Приёмка не должна стирать исходную выборку и адреса контрпримеров.
    if rule["evidence"].as_array().is_none_or(Vec::is_empty) {
        rule["evidence"] = json!([{"module":module,"method":method}]);
    }
    rule["acceptance_proof"] = json!({"module":module,"method":method});
    validate(conn, rule)
        .map_err(|_| format!("приёмка {id}: профиль не прошёл строгую проверку"))?;
    save_rules(&source, project, &commit, rules)
}

fn load_rules(src: &str, project: &str, commit: &str) -> Result<Vec<Value>, String> {
    let path = PathBuf::from(src)
        .join(".gyrfalcon")
        .join("project-practices.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let doc: Value = serde_json::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if doc.get("project").and_then(Value::as_str) != Some(project)
        || doc.get("source_commit").and_then(Value::as_str) != Some(commit)
    {
        return Err("существующий профиль относится к другому project или commit".into());
    }
    Ok(doc
        .get("rules")
        .and_then(Value::as_array)
        .cloned()
        .ok_or("в профиле нет rules")?)
}

fn save_rules(src: &str, project: &str, commit: &str, rules: Vec<Value>) -> Result<(), String> {
    save_rules_with_discovery(src, project, commit, rules, None)
}

fn save_rules_with_discovery(
    src: &str,
    project: &str,
    commit: &str,
    rules: Vec<Value>,
    discovery: Option<Value>,
) -> Result<(), String> {
    let path = PathBuf::from(src).join(".gyrfalcon");
    fs::create_dir_all(&path).map_err(|e| format!("не создан каталог профиля: {e}"))?;
    let profile_path = path.join("project-practices.json");
    let previous_discovery = if profile_path.is_file() {
        let raw = fs::read_to_string(&profile_path).map_err(|e| e.to_string())?;
        let previous: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        previous.get("discovery").cloned()
    } else {
        None
    };
    let json = serde_json::to_string_pretty(
        &json!({"version":1,"project":project,"source_commit":commit,
            "discovery":discovery.or(previous_discovery),"rules":rules}),
    )
    .map_err(|e| e.to_string())?;
    write_profile_atomic(&profile_path, &format!("{json}\n"), false)
}

fn slug(title: &str, line: usize) -> String {
    let s: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("doc-{}-{}", s.trim_matches('-'), line)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_profile_replace_preserves_readable_target() {
        let root = std::env::temp_dir().join(format!(
            "gyrfalcon-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let profile = root.join("project-practices.json");
        write_profile_atomic(&profile, "old", true).unwrap();
        write_profile_atomic(&profile, "ignored", true).unwrap();
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), "old");
        write_profile_atomic(&profile, "new", false).unwrap();
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), "new");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn legacy_and_weak_rule_are_not_accepted() {
        let c = Connection::open_in_memory().unwrap();
        let weak = json!({"status":"verified","legacy":false,"sample":9,"matches":9,"evidence":[{"module":"x","method":"y"}]});
        let legacy = json!({"status":"verified","legacy":true,"sample":10,"matches":10,"evidence":[{"module":"x","method":"y"}]});
        assert!(validate(&c, &weak).is_err());
        assert!(validate(&c, &legacy).is_err());
    }

    #[test]
    fn profile_returns_only_index_addressed_verified_rule() {
        let root = std::env::temp_dir().join(format!("gyrfalcon-practices-{}", std::process::id()));
        let dir = root.join(".gyrfalcon");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("project-practices.json"), r#"{
          "version":1,"project":"pilot","source_commit":"abc","rules":[
            {"id":"valid","title":"Valid","guidance":"Do it","status":"verified","legacy":false,"sample":10,"matches":9,"evidence":[{"module":"CommonModules/X/Ext/Module.bsl","method":"Check"}]},
            {"id":"old","title":"Old","guidance":"Do not publish","status":"verified","legacy":true,"sample":10,"matches":10,"evidence":[{"module":"CommonModules/X/Ext/Module.bsl","method":"Check"}]}
          ]
        }"#).unwrap();
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE index_meta(key TEXT,value TEXT); CREATE TABLE modules(id INTEGER PRIMARY KEY,rel_path TEXT); CREATE TABLE methods(module_id INTEGER,name TEXT);").unwrap();
        c.execute(
            "INSERT INTO index_meta VALUES('source_path',?1)",
            [&root.to_string_lossy()],
        )
        .unwrap();
        c.execute("INSERT INTO index_meta VALUES('git_commit','abc')", [])
            .unwrap();
        c.execute(
            "INSERT INTO modules VALUES(1,'CommonModules/X/Ext/Module.bsl')",
            [],
        )
        .unwrap();
        c.execute("INSERT INTO methods VALUES(1,'Check')", [])
            .unwrap();
        let got = profile(&c, &json!({"project":"pilot"})).unwrap();
        assert_eq!(got["metrics"]["accepted"], 1);
        assert!(got["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "valid"));
        assert_eq!(got["baseline"]["rules"], standard_baseline().unwrap().len());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn document_import_creates_non_published_candidates() {
        let root =
            std::env::temp_dir().join(format!("gyrfalcon-practices-import-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let doc = root.join("practices.md");
        std::fs::write(&doc, "# Проверять данные на сервере\n\nТекст практики\n").unwrap();
        assert_eq!(
            import_document(
                &root.to_string_lossy(),
                "pilot",
                "abc",
                &doc.to_string_lossy()
            )
            .unwrap(),
            1
        );
        let rules = load_rules(&root.to_string_lossy(), "pilot", "abc").unwrap();
        assert_eq!(rules[0]["status"], "candidate");
        assert_eq!(rules[0]["source"]["kind"], "documentation");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_add_cannot_skip_acceptance() {
        let root =
            std::env::temp_dir().join(format!("gyrfalcon-practices-manual-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        add_manual(
            &root.to_string_lossy(),
            "pilot",
            "abc",
            json!({
                "id":"manual","title":"Manual","guidance":"Do it","status":"verified",
                "legacy":false,"sample":10,"matches":10,
                "evidence":[{"module":"CommonModules/X/Ext/Module.bsl","method":"Check"}]
            }),
        )
        .unwrap();
        let rules = load_rules(&root.to_string_lossy(), "pilot", "abc").unwrap();
        assert_eq!(rules[0]["status"], "candidate");
        assert_eq!(rules[0]["sample"], 0);
        assert_eq!(rules[0]["evidence"], json!([]));
        assert_eq!(rules[0]["source"]["kind"], "manual");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_profile_call_discovers_modules_once_without_overwriting() {
        let root = std::env::temp_dir().join(format!(
            "gyrfalcon-practices-autodiscover-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let module_path = root.join("CommonModules/X/Ext/Module.bsl");
        std::fs::create_dir_all(module_path.parent().unwrap()).unwrap();
        std::fs::write(
            &module_path,
            "Процедура Check() Экспорт\n    Сообщить(\"ok\");\nКонецПроцедуры\n",
        )
        .unwrap();
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE index_meta(key TEXT,value TEXT);
            CREATE TABLE modules(id INTEGER PRIMARY KEY,rel_path TEXT,category TEXT,module_type TEXT,is_form INTEGER);
            CREATE TABLE methods(module_id INTEGER,name TEXT,is_export INTEGER,line INTEGER,end_line INTEGER,loc INTEGER);
            CREATE TABLE regions(module_id INTEGER,name TEXT,line INTEGER,end_line INTEGER);
            INSERT INTO index_meta VALUES('git_commit','abc');
            INSERT INTO modules VALUES(1,'CommonModules/X/Ext/Module.bsl','CommonModules','Module',0);
            INSERT INTO methods VALUES(1,'Check',1,2,4,3);
            INSERT INTO regions VALUES(1,'ПрограммныйИнтерфейс',1,5);").unwrap();
        c.execute(
            "INSERT INTO index_meta VALUES('source_path',?1)",
            [&root.to_string_lossy()],
        )
        .unwrap();
        c.execute(
            "INSERT INTO index_meta VALUES('built_at',?1)",
            [std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string()],
        )
        .unwrap();
        let first = profile(&c, &json!({"project":"pilot","view":"candidates"})).unwrap();
        assert_eq!(first["profile_auto_created"], true);
        assert_eq!(first["metrics"]["pending"], 0);
        assert_eq!(first["candidates"].as_array().unwrap().len(), 0);
        assert_eq!(first["discovery"]["modules_scanned"], 1);
        assert_eq!(first["discovery"]["source_modules_read"], 0);
        assert_eq!(
            first["baseline"]["rules"],
            standard_baseline().unwrap().len()
        );
        assert_eq!(first["baseline"]["project_copy"], false);
        let path = root.join(".gyrfalcon/project-practices.json");
        let before = std::fs::read_to_string(&path).unwrap();
        let stored: Value = serde_json::from_str(&before).unwrap();
        assert_eq!(stored["rules"].as_array().unwrap().len(), 0);
        assert!(stored["rules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["source"]["kind"] == "index-scan"));
        let second = profile(&c, &json!({"project":"pilot"})).unwrap();
        assert_eq!(second["profile_auto_created"], false);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        std::fs::write(
            &path,
            r#"{"version":1,"project":"pilot","source_commit":"abc","rules":[]}"#,
        )
        .unwrap();
        let populated = profile(&c, &json!({"project":"pilot"})).unwrap();
        assert_eq!(populated["profile_auto_created"], false);
        assert_eq!(populated["profile_auto_populated"], true);
        assert_eq!(populated["discovery"]["modules_scanned"], 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn acceptance_turns_document_candidate_into_profile_rule() {
        let root =
            std::env::temp_dir().join(format!("gyrfalcon-practices-accept-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".gyrfalcon")).unwrap();
        save_rules(&root.to_string_lossy(), "pilot", "abc", vec![json!({
            "id":"from-doc","title":"From doc","guidance":"Do it","status":"candidate","legacy":false,
            "sample":0,"matches":0,"evidence":[],"source":{"kind":"documentation"}
        })]).unwrap();
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE index_meta(key TEXT,value TEXT); CREATE TABLE modules(id INTEGER PRIMARY KEY,rel_path TEXT); CREATE TABLE methods(module_id INTEGER,name TEXT);").unwrap();
        c.execute(
            "INSERT INTO index_meta VALUES('source_path',?1)",
            [&root.to_string_lossy()],
        )
        .unwrap();
        c.execute("INSERT INTO index_meta VALUES('git_commit','abc')", [])
            .unwrap();
        c.execute(
            "INSERT INTO modules VALUES(1,'CommonModules/X/Ext/Module.bsl')",
            [],
        )
        .unwrap();
        c.execute("INSERT INTO methods VALUES(1,'Check')", [])
            .unwrap();
        accept(
            &c,
            "pilot",
            "from-doc",
            10,
            9,
            "CommonModules/X/Ext/Module.bsl",
            "Check",
        )
        .unwrap();
        assert_eq!(
            profile(&c, &json!({"project":"pilot"})).unwrap()["metrics"]["accepted"],
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn official_baseline_is_shared_and_not_a_project_profile_copy() {
        let baseline = standard_baseline().unwrap();
        assert!(baseline.len() >= 29);
        assert_eq!(
            baseline
                .iter()
                .filter(|r| r["approval"]["reviewer"] == "user")
                .count(),
            21
        );
        assert!(baseline.iter().all(|r| r["authority"] == "standard"));
        assert_eq!(baseline[0]["source"]["kind"], "1c-standard");
        let index = Connection::open_in_memory().unwrap();
        let no_project = empty(&index, "pilot", "unused", None, "index_commit_missing").unwrap();
        assert_eq!(
            no_project["rules"].as_array().unwrap().len(),
            baseline.len()
        );
        assert_eq!(no_project["baseline"]["project_copy"], false);
    }

    #[test]
    fn customer_requirement_suppresses_conflicting_standard_only_after_approval() {
        let c = Connection::open_in_memory().unwrap();
        let standard = standard_baseline()
            .unwrap()
            .into_iter()
            .find(|r| r["id"] == "std-456-no-commented-code")
            .unwrap();
        let customer = json!({"id":"preserve-old-code-and-mark-changes","title":"Customer",
            "guidance":"Preserve code", "status":"verified","legacy":false,
            "source":{"kind":"manual"},
            "approval":{"kind":"explicit","authority":"customer","reviewer":"user","reason":"contract"}});
        let customer = validate(&c, &customer).unwrap();
        let (active, suppressed) = resolve_conflicts(vec![standard, customer]);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0]["id"], "preserve-old-code-and-mark-changes");
        assert_eq!(suppressed[0]["rule"], "std-456-no-commented-code");
        assert!(!normative_authority_allowed(
            &json!({"source":{"kind":"index-scan"}}),
            "customer"
        ));
    }

    #[test]
    fn approved_customer_rule_survives_commit_change_without_rewriting_profile() {
        let root = std::env::temp_dir().join(format!(
            "gyrfalcon-practices-stale-normative-{}",
            std::process::id()
        ));
        let profile_path = root.join(".gyrfalcon/project-practices.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        let doc = json!({"version":1,"project":"pilot","source_commit":"old",
            "rules":[{"id":"preserve-old-code-and-mark-changes","title":"Customer",
                "guidance":"Preserve old code","status":"verified","legacy":false,
                "source":{"kind":"manual"},
                "approval":{"kind":"explicit","authority":"customer",
                    "reviewer":"user","reason":"contract"}},
                {"id":"observed","status":"verified","legacy":false,
                    "sample":10,"matches":10,"evidence":[],
                    "source":{"kind":"index-scan"}}]});
        let original = serde_json::to_string(&doc).unwrap();
        std::fs::write(&profile_path, &original).unwrap();
        let index = Connection::open_in_memory().unwrap();
        index
            .execute_batch(
                "CREATE TABLE index_meta(key TEXT,value TEXT);
            INSERT INTO index_meta VALUES('git_commit','new')",
            )
            .unwrap();
        index
            .execute(
                "INSERT INTO index_meta VALUES('source_path',?1)",
                [root.to_string_lossy().as_ref()],
            )
            .unwrap();
        let got = profile(&index, &json!({"project":"pilot"})).unwrap();
        assert_eq!(got["status"], "commit_mismatch");
        assert_eq!(got["metrics"]["accepted"], 1);
        assert_eq!(got["suppressed"][0]["rule"], "std-456-no-commented-code");
        assert!(got["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "preserve-old-code-and-mark-changes"));
        assert!(!got["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "observed"));
        assert_eq!(std::fs::read_to_string(&profile_path).unwrap(), original);
        std::fs::remove_dir_all(root).unwrap();
    }
}
