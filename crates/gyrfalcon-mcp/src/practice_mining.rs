//! Поиск проектных приёмов в полном корпусе BSL-модулей выбранного индекса.
//! Частота создаёт кандидата, но никогда не превращает его в норму без приёмки.

use gyrfalcon_parser::module;
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

const MIN_METHODS: usize = 10;
const MIN_MODULES: usize = 3;
const MAX_KEYS: usize = 3_000_000;
const MAX_PER_FAMILY: usize = 300;
const MAX_EXAMPLES: usize = 5;
const QUICK_MIN_SAMPLE: usize = 10;
const QUICK_MIN_SUPPORT: usize = 30;
const QUICK_MIN_CONSISTENCY: f64 = 0.90;

#[derive(Clone)]
struct QuickRegion {
    name: String,
    line: i64,
    end_line: Option<i64>,
}

#[derive(Default)]
struct QuickPair {
    observed: usize,
    evidence: Vec<(u64, Value)>,
    display: String,
    categories: HashSet<String>,
}

fn quick_evidence(stat: &QuickPair) -> Vec<Value> {
    let mut entries = stat.evidence.clone();
    entries.sort_by_key(|(rank, _)| *rank);
    entries.into_iter().map(|(_, address)| address).collect()
}

fn quick_scope(category: &str, module_type: &str, is_form: bool) -> String {
    if is_form {
        "формы/Module".to_string()
    } else {
        format!("{category}/{module_type}")
    }
}

fn quick_key(scope: &str, left: &str, right: &str) -> String {
    format!("{scope}\u{1f}{left}\u{1f}{right}")
}

fn record_quick_module(
    counts: &mut HashMap<String, QuickPair>,
    scope: &str,
    path: &str,
    first_method: Option<&str>,
    regions: &[QuickRegion],
) {
    let Some(method) = first_method else { return };
    let mut stack = Vec::new();
    let mut top = Vec::new();
    for region in regions {
        while stack.last().is_some_and(|end| *end < region.line) {
            stack.pop();
        }
        if stack.is_empty() {
            top.push(region);
        }
        if let Some(end) = region.end_line {
            stack.push(end);
        }
    }
    let mut name_counts = HashMap::new();
    for region in &top {
        *name_counts
            .entry(region.name.to_lowercase())
            .or_insert(0usize) += 1;
    }
    for (position, left) in top.iter().enumerate() {
        let left_key = left.name.to_lowercase();
        if name_counts[&left_key] != 1 {
            continue;
        }
        for right in &top[position + 1..] {
            let right_key = right.name.to_lowercase();
            if left_key == right_key || name_counts[&right_key] != 1 {
                continue;
            }
            let stat = counts
                .entry(quick_key(scope, &left_key, &right_key))
                .or_default();
            stat.observed += 1;
            if let Some(category) = path.split('/').next() {
                stat.categories.insert(category.to_string());
            }
            if stat.display.is_empty() {
                stat.display = format!("{} → {}", left.name, right.name);
            }
            // Детерминированная min-hash выборка по всему корпусу, а не первые
            // десять соседних форм одного подсистемного семейства.
            let digest = Sha256::digest(format!("{path}|{left_key}|{right_key}").as_bytes());
            let rank = u64::from_be_bytes(digest[..8].try_into().expect("sha256 prefix"));
            if stat.evidence.len() < QUICK_MIN_SAMPLE {
                stat.evidence.push((
                    rank,
                    json!({"module":path,"method":method,"line":left.line}),
                ));
            } else if let Some((position, maximum)) = stat
                .evidence
                .iter()
                .enumerate()
                .max_by_key(|(_, (rank, _))| *rank)
            {
                if rank < maximum.0 {
                    stat.evidence[position] = (
                        rank,
                        json!({"module":path,"method":method,"line":left.line}),
                    );
                }
            }
        }
    }
}

#[derive(Clone)]
struct GuardMethod {
    path: String,
    category: String,
    method: String,
    line: i64,
    end_line: i64,
    rank: u64,
}

// Дешёвый структурный предфильтр для одной гипотезы. Он НЕ доказывает
// семантическую эквивалентность проверяемой подсистемы и имени общего модуля.
fn guarded_module_line(body: &str, first_line: i64) -> Option<(i64, &'static str)> {
    let mut branches = Vec::new();
    let mut condition = String::new();
    let mut reading_condition = false;
    for (offset, raw) in body.lines().enumerate() {
        let line = raw.trim().to_lowercase();
        if line.starts_with("//") {
            continue;
        }
        if line.starts_with("если ") || line.starts_with("иначеесли ") {
            if line.starts_with("иначеесли ") {
                if let Some(last) = branches.last_mut() {
                    *last = false;
                }
            }
            condition.clear();
            condition.push_str(&line);
            reading_condition = true;
        } else if reading_condition {
            condition.push_str(&line);
        }
        if reading_condition && condition.contains(" тогда") {
            let guarded = condition.contains("подсистемасуществует(");
            if line.starts_with("иначеесли ") {
                if let Some(last) = branches.last_mut() {
                    *last = guarded;
                }
            } else {
                branches.push(guarded);
            }
            reading_condition = false;
            continue;
        }
        if line.starts_with("иначе") {
            if let Some(last) = branches.last_mut() {
                *last = false;
            }
            continue;
        }
        if line.starts_with("конецесли") {
            branches.pop();
            continue;
        }
        if branches.iter().any(|guarded| *guarded) && line.contains(".общиймодуль(") {
            return Some((first_line + offset as i64, "direct_condition"));
        }
    }
    let lines: Vec<_> = body
        .lines()
        .map(|line| line.trim().to_lowercase())
        .collect();
    // Частый эквивалент: результат проверки сохранён в булевой переменной.
    for (index, line) in lines.iter().enumerate() {
        let Some(call_at) = line.find(".подсистемасуществует(") else {
            continue;
        };
        let prefix = line[..call_at].trim();
        let Some(variable) = prefix.split('=').next().map(str::trim) else {
            continue;
        };
        if !prefix.contains('=') || variable.is_empty() || variable.contains(' ') {
            continue;
        }
        for (check, condition) in lines.iter().enumerate().skip(index + 1) {
            if condition != &format!("если {variable} тогда") {
                continue;
            }
            for (offset, inside) in lines.iter().enumerate().skip(check + 1) {
                if inside.starts_with("конецесли") || inside.starts_with("иначе") {
                    break;
                }
                if inside.contains(".общиймодуль(") {
                    return Some((first_line + offset as i64, "saved_guard_value"));
                }
            }
        }
    }
    // Второй эквивалент: отсутствие подсистемы завершает метод до получения
    // модуля. Это лишь локальная проверка, не общий анализ потока управления.
    for (index, line) in lines.iter().enumerate() {
        if !line.starts_with("если не ") || !line.contains(".подсистемасуществует(")
        {
            continue;
        }
        let mut returned = false;
        let mut end = None;
        for (offset, inside) in lines.iter().enumerate().skip(index + 1) {
            if inside.starts_with("иначе") {
                break;
            }
            if inside.starts_with("конецесли") {
                end = Some(offset);
                break;
            }
            returned |= inside.starts_with("возврат") || inside.starts_with("вызватьисключение");
        }
        if returned {
            if let Some(end) = end {
                for (offset, following) in lines.iter().enumerate().skip(end + 1) {
                    if following.contains(".общиймодуль(") {
                        return Some((first_line + offset as i64, "early_exit"));
                    }
                }
            }
        }
    }
    None
}

fn literal_api_arguments(body: &str, call: &str) -> HashSet<String> {
    let lower = body.to_lowercase();
    let mut names = HashSet::new();
    let mut offset = 0usize;
    while let Some(found) = lower[offset..].find(call) {
        let from = offset + found + call.len();
        let tail = &lower[from..];
        let Some(open) = tail.find('"') else {
            break;
        };
        let rest = &tail[open + 1..];
        let Some(close) = rest.find('"') else {
            break;
        };
        if open < 16 && close > 0 && close < 150 {
            names.insert(rest[..close].to_string());
        }
        offset = from;
    }
    names
}

fn mine_optional_subsystem_guard(
    conn: &Connection,
    source_root: &str,
) -> Result<(Option<Value>, Value), String> {
    let has_calls: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='calls')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if !has_calls {
        return Ok((None, json!({"available":false,"source_modules_read":0})));
    }
    let mut stmt = conn.prepare(
        "WITH hits AS (
            SELECT caller_id,
              MAX(CASE WHEN callee_name='ОбщегоНазначения.ПодсистемаСуществует' THEN 1 ELSE 0 END) AS sg,
              MAX(CASE WHEN callee_name='ОбщегоНазначения.ОбщийМодуль' THEN 1 ELSE 0 END) AS sm,
              MAX(CASE WHEN callee_name='ОбщегоНазначенияКлиент.ПодсистемаСуществует' THEN 1 ELSE 0 END) AS cg,
              MAX(CASE WHEN callee_name='ОбщегоНазначенияКлиент.ОбщийМодуль' THEN 1 ELSE 0 END) AS cm
            FROM calls WHERE callee_name IN (
              'ОбщегоНазначения.ПодсистемаСуществует','ОбщегоНазначения.ОбщийМодуль',
              'ОбщегоНазначенияКлиент.ПодсистемаСуществует','ОбщегоНазначенияКлиент.ОбщийМодуль')
            GROUP BY caller_id
         )
         SELECT mo.rel_path,COALESCE(mo.category,''),m.name,m.line,m.end_line
         FROM hits h JOIN methods m ON m.id=h.caller_id
         JOIN modules mo ON mo.id=m.module_id
         WHERE (h.sg=1 AND h.sm=1) OR (h.cg=1 AND h.cm=1)
         ORDER BY mo.rel_path,m.line",
    ).map_err(|e| format!("не прочитаны пары вызовов из индекса: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut by_category: HashMap<String, Vec<GuardMethod>> = HashMap::new();
    let mut indexed_pairs = 0usize;
    for row in rows {
        let (path, category, method, line, end_line) = row.map_err(|e| e.to_string())?;
        let (Some(line), Some(end_line)) = (line, end_line) else {
            continue;
        };
        if line < 1 || end_line < line {
            continue;
        }
        indexed_pairs += 1;
        let digest = Sha256::digest(format!("{path}|{method}|{line}").as_bytes());
        let rank = u64::from_be_bytes(digest[..8].try_into().expect("sha256 prefix"));
        by_category
            .entry(category.clone())
            .or_default()
            .push(GuardMethod {
                path,
                category,
                method,
                line,
                end_line,
                rank,
            });
    }
    for methods in by_category.values_mut() {
        methods.sort_by_key(|entry| entry.rank);
    }
    let mut categories: Vec<_> = by_category.into_iter().collect();
    categories.sort_by(|a, b| a.0.cmp(&b.0));
    let mut selected = Vec::new();
    let mut offset = 0usize;
    while selected.len() < 100 {
        let mut added = false;
        for (_, methods) in &categories {
            if let Some(method) = methods.get(offset) {
                selected.push(method.clone());
                added = true;
                if selected.len() == 100 {
                    break;
                }
            }
        }
        if !added {
            break;
        }
        offset += 1;
    }
    let mut sources = HashMap::new();
    let mut evidence = Vec::new();
    let mut counterexamples = Vec::new();
    let mut category_spread = HashSet::new();
    let mut subsystem_literals = HashSet::new();
    let mut module_literals = HashSet::new();
    let mut pattern_counts: HashMap<&str, usize> = HashMap::new();
    let mut matched = 0usize;
    for entry in &selected {
        if !sources.contains_key(&entry.path) {
            let path = safe_source_path(Path::new(source_root), &entry.path)?;
            let source = std::fs::read_to_string(&path)
                .map_err(|e| format!("не прочитан модуль для выборки {}: {e}", entry.path))?;
            sources.insert(entry.path.clone(), source);
        }
        let source = &sources[&entry.path];
        let lines: Vec<_> = source.lines().collect();
        let from = (entry.line - 1) as usize;
        let to = (entry.end_line as usize).min(lines.len());
        if from >= to {
            return Err(format!(
                "неверные границы метода {}::{}",
                entry.path, entry.method
            ));
        }
        let body = lines[from..to].join("\n");
        let address = json!({"module":entry.path,"method":entry.method,"line":entry.line});
        if let Some((at, pattern)) = guarded_module_line(&body, entry.line) {
            matched += 1;
            category_spread.insert(entry.category.clone());
            subsystem_literals.extend(literal_api_arguments(&body, ".подсистемасуществует("));
            module_literals.extend(literal_api_arguments(&body, ".общиймодуль("));
            *pattern_counts.entry(pattern).or_default() += 1;
            if evidence.len() < QUICK_MIN_SAMPLE {
                evidence.push(json!({"module":entry.path,"method":entry.method,
                    "line":at,"pattern":pattern}));
            }
        } else if counterexamples.len() < QUICK_MIN_SAMPLE {
            counterexamples.push(address);
        }
    }
    let baseline: Value = serde_json::from_str(include_str!("one_c_standards.json"))
        .map_err(|e| format!("не прочитан общий базис практик: {e}"))?;
    let exact_standard_ids: Vec<_> = baseline["rules"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|rule| {
            let wording = format!(
                "{} {}",
                rule["title"].as_str().unwrap_or(""),
                rule["guidance"].as_str().unwrap_or("")
            )
            .to_lowercase();
            (wording.contains("подсистемасуществует") && wording.contains("общиймодуль"))
                .then(|| rule["id"].as_str().unwrap_or("").to_string())
        })
        .collect();
    let summary = json!({"available":true,"indexed_cooccurrences":indexed_pairs,
        "sample":selected.len(),"guarded_in_sample":matched,
        "category_spread":category_spread.len(),"source_modules_read":sources.len(),
        "detected_forms":pattern_counts,
        "distinct_subsystem_literals_in_sample":subsystem_literals.len(),
        "distinct_module_literals_in_sample":module_literals.len(),
        "exact_baseline_api_matches":exact_standard_ids,
        "sampling":"deterministic hash, round-robin by module category"});
    if evidence.len() < QUICK_MIN_SAMPLE
        || category_spread.len() < 3
        || !exact_standard_ids.is_empty()
    {
        return Ok((None, summary));
    }
    let signature = "ПодсистемаСуществует → ОбщийМодуль (условная ветка)";
    let digest = Sha256::digest(signature.as_bytes());
    let short = format!("{digest:x}");
    let candidate = json!({
        "id":format!("mined-api-guard-{}", &short[..16]),
        "title":"Проверка подключаемой подсистемы перед вызовом её общего модуля",
        "guidance":"При обращении к общему модулю необязательной подсистемы проверить её наличие и выполнять получение и вызов модуля внутри условной ветки.",
        "status":"candidate","legacy":false,
        "sample":selected.len(),"matches":matched,
        "scope":"BSL-модули с условно подключаемыми подсистемами",
        "evidence":evidence,"counterexamples":counterexamples,
        "exceptions":"Совпадение имён API и лексическая вложенность в условие не доказывают корректность имени проверяемой подсистемы; проверить конкретную интеграцию и обратные случаи до приёмки.",
        "observation":summary,
        "source":{"kind":"indexed-api-pair-with-sampled-control-flow",
            "family":"calls","scope":"indexed BSL modules","signature":signature},
        "baseline_relation":{"status":"no-exact-api-match-semantic-review-required",
            "exact_standard_ids":exact_standard_ids,
            "note":"Точного сочетания API в одной карточке базиса нет; смысловое совпадение всё ещё требует проверки."}
    });
    Ok((Some(candidate), summary))
}

/// Быстрый проход по *индексу BSL-модулей*: не перечитывает исходники и не
/// выдаёт сырую частоту имени или двух вызовов за проектную практику.
fn mine_quick(conn: &Connection, source_root: &str) -> Result<MiningResult, String> {
    let mut stmt = conn
        .prepare(
            "SELECT mo.id,mo.rel_path,COALESCE(mo.category,''),COALESCE(mo.module_type,''),
         COALESCE(mo.is_form,0),
         (SELECT name FROM methods m WHERE m.module_id=mo.id ORDER BY line LIMIT 1),
         r.name,r.line,r.end_line
         FROM modules mo LEFT JOIN regions r ON r.module_id=mo.id
         WHERE mo.rel_path LIKE '%.bsl' ORDER BY mo.id,r.line",
        )
        .map_err(|e| format!("не прочитаны области BSL-модулей: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)? != 0,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut counts: HashMap<String, QuickPair> = HashMap::new();
    let mut current_id = None;
    let mut current_path = String::new();
    let mut current_scope = String::new();
    let mut current_method: Option<String> = None;
    let mut current_regions = Vec::new();
    let mut modules = 0usize;
    let mut regions = 0usize;
    for row in rows {
        let (id, path, category, module_type, is_form, method, name, line, end_line) =
            row.map_err(|e| e.to_string())?;
        if current_id != Some(id) {
            if current_id.is_some() {
                record_quick_module(
                    &mut counts,
                    &current_scope,
                    &current_path,
                    current_method.as_deref(),
                    &current_regions,
                );
                current_regions.clear();
            }
            modules += 1;
            current_id = Some(id);
            current_path = path;
            current_scope = quick_scope(&category, &module_type, is_form);
            current_method = method;
        }
        if let (Some(name), Some(line)) = (name, line) {
            regions += 1;
            current_regions.push(QuickRegion {
                name,
                line,
                end_line,
            });
        }
    }
    if current_id.is_some() {
        record_quick_module(
            &mut counts,
            &current_scope,
            &current_path,
            current_method.as_deref(),
            &current_regions,
        );
    }
    let raw_patterns = counts.len();
    let mut candidates = Vec::new();
    let mut visited = HashSet::new();
    for (key, stat) in &counts {
        let mut parts = key.split('\u{1f}');
        let (Some(scope), Some(left), Some(right)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let reverse_key = quick_key(scope, right, left);
        let reverse = counts.get(&reverse_key);
        let inverse = reverse.map_or(0, |r| r.observed);
        let sample = stat.observed + inverse;
        let pair_id = if left < right {
            quick_key(scope, left, right)
        } else {
            reverse_key
        };
        if visited.contains(&pair_id)
            || stat.observed < inverse
            || sample < QUICK_MIN_SAMPLE
            || stat.observed < QUICK_MIN_SUPPORT
            || (scope == "формы/Module" && stat.categories.len() < 3)
            || stat.evidence.len() < QUICK_MIN_SAMPLE
            || (stat.observed as f64) / (sample as f64) < QUICK_MIN_CONSISTENCY
        {
            continue;
        }
        visited.insert(pair_id);
        let digest = Sha256::digest(key.as_bytes());
        let short = format!("{digest:x}");
        let counterexamples = reverse.map_or_else(Vec::new, quick_evidence);
        let exceptions = if inverse == 0 {
            "Применять только если обе области нужны модулю; обратный порядок среди модулей с обеими областями не обнаружен.".to_string()
        } else {
            format!("Не переносить порядок механически: в {inverse} модулях с обеими областями найден обратный порядок; адреса приведены в counterexamples.")
        };
        candidates.push(json!({
            "id":format!("mined-region-order-{}", &short[..16]),
            "title":format!("Порядок областей: {}",stat.display),
            "guidance":format!("В BSL-модулях группы {scope}, если нужны обе области, размещать {}. При доработке существующего модуля учитывать его текущую структуру.",stat.display),
            "status":"candidate","legacy":false,
            "sample":sample,"matches":stat.observed,
            "evidence":quick_evidence(stat),"counterexamples":counterexamples,
            "exceptions":exceptions,"scope":scope,
            "observation":{"evaluated_modules":sample,"matching_modules":stat.observed,
                "reverse_modules":inverse,"category_spread":stat.categories.len(),
                "order_consistency":stat.observed as f64 / sample as f64},
            "source":{"kind":"indexed-region-order","family":"regions",
                "scope":"indexed BSL modules","signature":format!("{left} → {right}")}
        }));
    }
    candidates.sort_by(|a, b| {
        let a_n = a["matches"].as_u64().unwrap_or(0);
        let b_n = b["matches"].as_u64().unwrap_or(0);
        b_n.cmp(&a_n)
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    candidates.truncate(MAX_PER_FAMILY * 3);
    // Все попарные порядки — свидетельства к одному общему стандарту
    // «Разделы программного модуля», а не независимые правила проекта.
    let region_observations = candidates.len();
    let region_ids: Vec<_> = candidates
        .iter()
        .filter_map(|card| card["id"].as_str())
        .collect();
    let (guard_candidate, guard_coverage) = mine_optional_subsystem_guard(conn, source_root)?;
    let candidates: Vec<Value> = guard_candidate.into_iter().collect();
    Ok(MiningResult {
        coverage: json!({
            "kind":"indexed-bsl-practice-hypotheses","depth":"quick","completed":true,
            "modules_indexed":modules,"modules_scanned":modules,"regions_indexed":regions,
            "source_modules_read":guard_coverage["source_modules_read"],
            "raw_order_signatures":raw_patterns,
            "baseline_evidence":{"standard_id":"module-regions",
                "region_order_observations":region_observations,"observation_ids":region_ids,
                "new_project_practices":0},
            "api_guard_sample":guard_coverage,
            "emitted":{"region_order":0,"api_guard":candidates.len(),
                "method_names":0,"raw_calls":0},
            "minimum_sample":QUICK_MIN_SAMPLE,"minimum_support":QUICK_MIN_SUPPORT,
            "minimum_consistency":QUICK_MIN_CONSISTENCY,
            "minimum_form_categories":3
        }),
        candidates,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MiningDepth {
    Quick,
    Full,
}

impl MiningDepth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Full => "full",
        }
    }
}

pub struct MiningResult {
    pub candidates: Vec<Value>,
    pub coverage: Value,
}

#[derive(Default)]
struct Count {
    observed: usize,
    modules: usize,
    last_module: i64,
    evidence: Vec<Value>,
}

#[derive(Clone)]
struct Item {
    family: &'static str,
    context: String,
    signature: String,
    observed: usize,
    modules: usize,
    population: usize,
    evidence: Vec<Value>,
}

fn context(category: &str, module_type: &str) -> String {
    format!("{category}/{module_type}")
}

#[derive(Clone, Copy)]
struct EvidenceRef<'a> {
    module_id: i64,
    path: &'a str,
    method: &'a str,
    line: u32,
}

fn note_pattern(
    counts: &mut HashMap<String, Count>,
    family: &'static str,
    context: &str,
    signature: &str,
    evidence: EvidenceRef<'_>,
) -> Result<(), String> {
    let key = format!("{family}\u{1f}{context}\u{1f}{signature}");
    if !counts.contains_key(&key) && counts.len() >= MAX_KEYS {
        return Err(format!(
            "поиск остановлен: более {MAX_KEYS} различных фрагментов; профиль не записан"
        ));
    }
    let stat = counts.entry(key).or_default();
    stat.observed += 1;
    if stat.last_module != evidence.module_id {
        stat.modules += 1;
        stat.last_module = evidence.module_id;
        // Единичные отпечатки составляют основной объём: адреса для них
        // не храним, пока шаблон не повторился хотя бы в трёх модулях.
        if stat.modules >= 3 && stat.evidence.len() < MAX_EXAMPLES {
            stat.evidence.push(json!({"module":evidence.path,
                "method":evidence.method,"line":evidence.line}));
        }
    }
    Ok(())
}

fn call_name(name: &str) -> Option<String> {
    let normalized = name.trim().to_lowercase();
    if normalized.is_empty() || normalized.len() > 180 {
        return None;
    }
    Some(normalized)
}

fn ast_shape(signature: &str) -> Option<String> {
    let structural = [
        "if_statement",
        "try_statement",
        "for_statement",
        "for_each_statement",
        "while_statement",
    ];
    if !structural
        .iter()
        .any(|prefix| signature.starts_with(prefix))
    {
        return None;
    }
    // В AST-семействе ищем именно синтаксическую форму: конкретные API-имена
    // уже сохранены в независимом проходе последовательностей вызовов.
    let mut out = String::with_capacity(signature.len());
    let mut rest = signature;
    while let Some(at) = rest.find("call:") {
        out.push_str(&rest[..at]);
        out.push_str("call:*");
        rest = &rest[at + 5..];
        let end = rest.find([',', ')']).unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    Some(out)
}

fn add_sequences(
    counts: &mut HashMap<String, Count>,
    calls: &[module::Call],
    category_context: &str,
    named_context: Option<&str>,
    evidence: EvidenceRef<'_>,
    depth: MiningDepth,
) -> Result<(), String> {
    let names: Vec<_> = calls
        .iter()
        .filter_map(|c| call_name(&c.name).map(|name| (name, c.line)))
        .collect();
    if names.len() < 2 {
        return Ok(());
    }
    let mut seen = HashSet::new();
    let widths: &[usize] = if depth == MiningDepth::Full {
        &[2, 3]
    } else {
        &[2]
    };
    for &width in widths {
        for window in names.windows(width) {
            if window.iter().all(|call| call.0 == window[0].0)
                || !window.iter().any(|call| call.0.contains('.'))
            {
                continue;
            }
            let signature = window
                .iter()
                .map(|call| call.0.as_str())
                .collect::<Vec<_>>()
                .join(" → ");
            if !seen.insert(signature.clone()) {
                continue;
            }
            let at_call = EvidenceRef {
                line: window[0].1,
                ..evidence
            };
            note_pattern(counts, "calls", category_context, &signature, at_call)?;
            if let Some(named) = named_context {
                note_pattern(counts, "calls", named, &signature, at_call)?;
            }
        }
    }
    Ok(())
}

fn safe_source_path(root: &Path, rel_path: &str) -> Result<std::path::PathBuf, String> {
    let relative = Path::new(rel_path);
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("некорректный путь модуля в индексе: {rel_path}"));
    }
    Ok(root.join(relative))
}

pub fn mine(
    conn: &Connection,
    source_root: &str,
    depth: MiningDepth,
) -> Result<MiningResult, String> {
    if depth == MiningDepth::Quick {
        return mine_quick(conn, source_root);
    }
    let mut frequent_names = HashSet::new();
    let mut name_stmt = conn
        .prepare(
            "SELECT COALESCE(mo.category,''),COALESCE(mo.module_type,''),LOWER(m.name) \
         FROM methods m JOIN modules mo ON mo.id=m.module_id \
         WHERE mo.rel_path LIKE '%.bsl' \
         GROUP BY mo.category,mo.module_type,LOWER(m.name) HAVING COUNT(*)>=10",
        )
        .map_err(|e| format!("не прочитаны контексты методов: {e}"))?;
    let names = name_stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| format!("не прочитаны контексты методов: {e}"))?;
    for name in names {
        let (category, module_type, method) = name.map_err(|e| e.to_string())?;
        frequent_names.insert(format!(
            "methods/{}/{}/{}",
            category,
            module_type,
            method.to_lowercase()
        ));
    }
    let mut counts: HashMap<String, Count> = HashMap::new();
    let mut populations: HashMap<String, usize> = HashMap::new();
    let mut modules_total = 0usize;
    let mut modules_read = 0usize;
    let mut modules_parse_errors = 0usize;
    let mut methods_read = 0usize;
    let mut calls_read = 0usize;
    let mut regions_read = 0usize;
    let mut ast_methods_truncated = 0usize;
    let mut ast_methods_seen = 0usize;
    let mut missing = Vec::new();
    let mut parse_errors = Vec::new();
    let root = Path::new(source_root);
    let mut stmt = conn
        .prepare(
            "SELECT id,rel_path,COALESCE(category,''),COALESCE(module_type,'') \
         FROM modules WHERE rel_path LIKE '%.bsl' ORDER BY id",
        )
        .map_err(|e| format!("не прочитан список модулей: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| format!("не прочитан список модулей: {e}"))?;
    for row in rows {
        let (module_id, rel_path, category, module_type) = row.map_err(|e| e.to_string())?;
        modules_total += 1;
        let full_path = safe_source_path(root, &rel_path)?;
        let source = match std::fs::read_to_string(&full_path) {
            Ok(s) => s,
            Err(e) => {
                missing.push(json!({"module":rel_path,"error":e.to_string()}));
                continue;
            }
        };
        let parsed = match module::parse(&source) {
            Ok(p) if !p.has_errors => p,
            Ok(_) => {
                modules_parse_errors += 1;
                parse_errors.push(rel_path);
                continue;
            }
            Err(e) => {
                modules_parse_errors += 1;
                parse_errors.push(format!("{rel_path}: {e}"));
                continue;
            }
        };
        modules_read += 1;
        methods_read += parsed.methods.len();
        calls_read += parsed.calls.len();
        regions_read += parsed.regions.len();
        let broad = context(&category, &module_type);
        let region_context = format!("regions/{broad}");
        *populations.entry(region_context.clone()).or_default() += 1;
        let first_method = parsed.methods.first();
        // Вложенная область не является следующим разделом модуля.
        let mut region_stack: Vec<u32> = Vec::new();
        let mut top_regions = Vec::new();
        for region in &parsed.regions {
            while region_stack.last().is_some_and(|end| *end < region.line) {
                region_stack.pop();
            }
            if region_stack.is_empty() {
                top_regions.push(region);
            }
            if let Some(end) = region.end_line {
                region_stack.push(end);
            }
        }
        let mut region_seen = HashSet::new();
        let region_widths: &[usize] = if depth == MiningDepth::Full {
            &[1, 2, 3]
        } else {
            &[1, 2]
        };
        for &width in region_widths {
            for window in top_regions.windows(width) {
                let signature = window
                    .iter()
                    .map(|r| r.name.to_lowercase())
                    .collect::<Vec<_>>()
                    .join(" → ");
                if !region_seen.insert(signature.clone()) {
                    continue;
                }
                if let Some(method) = first_method {
                    note_pattern(
                        &mut counts,
                        "regions",
                        &region_context,
                        &signature,
                        EvidenceRef {
                            module_id,
                            path: &rel_path,
                            method: &method.name,
                            line: window[0].line,
                        },
                    )?;
                }
            }
        }
        for method in &parsed.methods {
            let method_context = format!("methods/{broad}");
            let named_context = format!("methods/{broad}/{}", method.name.to_lowercase());
            *populations.entry(method_context.clone()).or_default() += 1;
            note_pattern(
                &mut counts,
                "method-names",
                &method_context,
                &method.name.to_lowercase(),
                EvidenceRef {
                    module_id,
                    path: &rel_path,
                    method: &method.name,
                    line: method.line_start,
                },
            )?;
            let named = frequent_names
                .contains(&named_context)
                .then_some(named_context.as_str());
            if named.is_some() {
                *populations.entry(named_context.clone()).or_default() += 1;
            }
            let from = parsed.calls.partition_point(|c| c.line < method.line_start);
            let to = parsed.calls.partition_point(|c| c.line <= method.line_end);
            add_sequences(
                &mut counts,
                &parsed.calls[from..to],
                &method_context,
                named,
                EvidenceRef {
                    module_id,
                    path: &rel_path,
                    method: &method.name,
                    line: method.line_start,
                },
                depth,
            )?;
        }
        // AST-проход независим от имён вызываемых процедур и видит повторяющиеся
        // синтаксические конструкции. Неполный разбор модуля выше исключён.
        if depth == MiningDepth::Full {
            let ast = crate::practice_ast::fragments_with_stats(&source)?;
            ast_methods_truncated += ast.methods_truncated;
            ast_methods_seen += ast.methods_seen;
            let mut seen = HashSet::new();
            for fragment in ast.fragments {
                let Some(shape) = ast_shape(&fragment.signature) else {
                    continue;
                };
                let method_idx = parsed
                    .methods
                    .partition_point(|m| m.line_start <= fragment.line);
                let Some(method) = method_idx
                    .checked_sub(1)
                    .and_then(|i| parsed.methods.get(i))
                    .filter(|m| m.name == fragment.method_name && fragment.line <= m.line_end)
                else {
                    continue;
                };
                let key = (method.line_start, shape.clone());
                if !seen.insert(key) {
                    continue;
                }
                let ast_context = format!("methods/{broad}");
                note_pattern(
                    &mut counts,
                    "ast",
                    &ast_context,
                    &shape,
                    EvidenceRef {
                        module_id,
                        path: &rel_path,
                        method: &method.name,
                        line: fragment.line,
                    },
                )?;
            }
        }
    }
    // При недочитанных модулях результат не выдаётся за полный. Не создаём
    // автоматически профиль из частичного корпуса.
    if !missing.is_empty() || modules_parse_errors > 0 {
        return Err(format!(
            "корпус прочитан не полностью: модулей {modules_total}, \
            прочитано {modules_read}, отсутствует {}, ошибок разбора {}. \
            Профиль не записан; первые пропуски: {:?}; первые ошибки: {:?}",
            missing.len(),
            modules_parse_errors,
            &missing[..missing.len().min(3)],
            &parse_errors[..parse_errors.len().min(3)]
        ));
    }
    let all_keys = counts.len();
    let mut eligible = [0usize; 4];
    let mut grouped: [Vec<Item>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for (key, stat) in counts {
        let mut parts = key.split('\u{1f}');
        let (Some(family), Some(ctx), Some(signature)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let family_index = match family {
            "regions" => 0,
            "method-names" => 1,
            "calls" => 2,
            "ast" => 3,
            _ => continue,
        };
        let population = populations.get(ctx).copied().unwrap_or(0);
        if stat.observed < MIN_METHODS || stat.modules < MIN_MODULES || population == 0 {
            continue;
        }
        eligible[family_index] += 1;
        grouped[family_index].push(Item {
            family: match family_index {
                0 => "regions",
                1 => "method-names",
                2 => "calls",
                _ => "ast",
            },
            context: ctx.to_string(),
            signature: signature.to_string(),
            observed: stat.observed,
            modules: stat.modules,
            population,
            evidence: stat.evidence,
        });
    }
    let mut candidates = Vec::new();
    let mut emitted = [0usize; 4];
    for (i, family) in grouped.iter_mut().enumerate() {
        family.sort_by(|a, b| {
            let a_score = a.modules as f64 * a.observed as f64 / a.population as f64;
            let b_score = b.modules as f64 * b.observed as f64 / b.population as f64;
            b_score
                .total_cmp(&a_score)
                .then_with(|| b.modules.cmp(&a.modules))
                .then_with(|| b.observed.cmp(&a.observed))
                .then_with(|| a.context.cmp(&b.context))
                .then_with(|| a.signature.cmp(&b.signature))
        });
        for item in family.iter().take(MAX_PER_FAMILY) {
            let digest = Sha256::digest(
                format!("{}|{}|{}", item.family, item.context, item.signature).as_bytes(),
            );
            let short = format!("{digest:x}");
            let title = match item.family {
                "regions" => format!("Порядок областей: {}", item.signature),
                "method-names" => format!("Повторяющаяся точка входа: {}", item.signature),
                "calls" => format!("Последовательность вызовов: {}", item.signature),
                _ => format!("Повторяющийся синтаксический фрагмент: {}", item.signature),
            };
            let guidance = format!(
                "При доработке модулей группы {} проверить применимость \
                шаблона «{}»; частота сама по себе не делает его обязательным.",
                item.context, item.signature
            );
            candidates.push(json!({
                "id":format!("mined-{}-{}", item.family, &short[..16]),
                "title":title,"guidance":guidance,"status":"candidate","legacy":false,
                "sample":0,"matches":0,"evidence":item.evidence,
                "scope":item.context,"exceptions":"Проверить по адресуемым контрпримерам до приёмки",
                "observation":{"population":item.population,"observed":item.observed,
                    "modules":item.modules},
                "source":{"kind":"index-scan","family":item.family,
                    "scope":"all indexed BSL modules","signature":item.signature}
            }));
            emitted[i] += 1;
        }
    }
    Ok(MiningResult {
        candidates,
        coverage: json!({
            "kind":"modules-only-practice-mining","depth":depth.as_str(),
            "completed":true,"ast_bounded":ast_methods_truncated>0,
            "modules_indexed":modules_total,"modules_read":modules_read,
                "methods_parsed":methods_read,"calls_parsed":calls_read,"regions_parsed":regions_read,
                "ast_methods_seen":ast_methods_seen,"ast_methods_truncated":ast_methods_truncated,
            "parse_errors":modules_parse_errors,"missing_modules":missing.len(),
            "distinct_signatures":all_keys,
            "eligible":{"regions":eligible[0],"method_names":eligible[1],
                "calls":eligible[2],"ast":eligible[3]},
            "emitted":{"regions":emitted[0],"method_names":emitted[1],
                "calls":emitted[2],"ast":emitted[3]},
            "limit_per_family":MAX_PER_FAMILY
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_module_patterns_have_stable_ids_and_addresses() {
        let root = std::env::temp_dir().join(format!(
            "gyrfalcon-mining-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE modules(id INTEGER PRIMARY KEY,rel_path TEXT,
            category TEXT,module_type TEXT,is_form INTEGER);
            CREATE TABLE methods(id INTEGER PRIMARY KEY,module_id INTEGER,name TEXT,line INTEGER);
            CREATE TABLE regions(id INTEGER PRIMARY KEY,module_id INTEGER,name TEXT,
            line INTEGER,end_line INTEGER);",
        )
        .unwrap();
        for id in 1..=31 {
            let rel = format!("CommonModules/Test{id}/Ext/Module.bsl");
            let path = root.join(&rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                "#Область ПрограммныйИнтерфейс\n\
                Процедура Проверка() Экспорт\n\
                Если Истина Тогда\n\
                Запрос.УстановитьПараметр(\"Ключ\", 1);\n\
                Запрос.Выполнить();\n\
                КонецЕсли;\n\
                КонецПроцедуры\n\
                #КонецОбласти\n",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO modules VALUES(?1,?2,'CommonModules','Module',0)",
                rusqlite::params![id, rel],
            )
            .unwrap();
            conn.execute("INSERT INTO methods VALUES(?1,?1,'Проверка',2)", [id])
                .unwrap();
            let order = if id == 31 {
                [
                    ("СлужебныеПроцедурыИФункции", 1),
                    ("ПрограммныйИнтерфейс", 10),
                ]
            } else {
                [
                    ("ПрограммныйИнтерфейс", 1),
                    ("СлужебныеПроцедурыИФункции", 10),
                ]
            };
            for (offset, (name, line)) in order.into_iter().enumerate() {
                conn.execute(
                    "INSERT INTO regions VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![id * 2 + offset, id, name, line, line + 5],
                )
                .unwrap();
            }
        }
        let first = mine(&conn, &root.to_string_lossy(), MiningDepth::Quick).unwrap();
        assert_eq!(first.coverage["modules_scanned"], 31);
        assert_eq!(first.coverage["source_modules_read"], 0);
        assert!(first.candidates.is_empty());
        assert_eq!(
            first.coverage["baseline_evidence"]["standard_id"],
            "module-regions"
        );
        assert_eq!(
            first.coverage["baseline_evidence"]["region_order_observations"],
            1
        );
        assert_eq!(first.coverage["emitted"]["region_order"], 0);
        let second = mine(&conn, &root.to_string_lossy(), MiningDepth::Quick).unwrap();
        assert_eq!(first.candidates, second.candidates);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn optional_subsystem_guard_requires_module_inside_branch() {
        let inside = "Процедура Тест()\nЕсли ОбщегоНазначения.ПодсистемаСуществует(\"А\") Тогда\n  Модуль = ОбщегоНазначения.ОбщийМодуль(\"Б\");\nКонецЕсли;\nКонецПроцедуры";
        let outside = "Процедура Тест()\nЕсли ОбщегоНазначения.ПодсистемаСуществует(\"А\") Тогда\n  Возврат;\nКонецЕсли;\nМодуль = ОбщегоНазначения.ОбщийМодуль(\"Б\");\nКонецПроцедуры";
        assert_eq!(
            guarded_module_line(inside, 10),
            Some((12, "direct_condition"))
        );
        assert_eq!(guarded_module_line(outside, 10), None);
        let saved = "Процедура Тест()\nЕстьПодсистема = ОбщегоНазначения.ПодсистемаСуществует(\"А\");\nЕсли ЕстьПодсистема Тогда\nМодуль = ОбщегоНазначения.ОбщийМодуль(\"Б\");\nКонецЕсли;\nКонецПроцедуры";
        let early = "Процедура Тест()\nЕсли Не ОбщегоНазначения.ПодсистемаСуществует(\"А\") Тогда\nВозврат;\nКонецЕсли;\nМодуль = ОбщегоНазначения.ОбщийМодуль(\"Б\");\nКонецПроцедуры";
        assert_eq!(
            guarded_module_line(saved, 1),
            Some((4, "saved_guard_value"))
        );
        assert_eq!(guarded_module_line(early, 1), Some((5, "early_exit")));
        assert_eq!(
            literal_api_arguments(inside, ".подсистемасуществует(").len(),
            1
        );
        assert!(literal_api_arguments(inside, ".подсистемасуществует(").contains("а"));
    }
}
