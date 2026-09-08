//! Узкий индекс официального синтакс-помощника платформы 1С.
//!
//! Источник — локальный `shcntx_ru.hbk` той версии платформы, которая
//! установлена на машине. Наружу выходят только точные страницы API:
//! синтаксис, краткое описание, доступность и короткий пример. Полные HTML
//! и слабые «лучшие из плохих» совпадения не возвращаются.

use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const MAX_SECTION: usize = 800;
const MIN_TERM_MATCHES: usize = 2;
const TARGET_SCORE_WINDOW: usize = 20;

#[derive(Debug, Clone)]
struct Item {
    title: String,
    owner: String,
    name: String,
    kind: String,
    syntax: String,
    description: String,
    availability: String,
    example: String,
    path: String,
}

#[derive(Debug)]
struct Catalog {
    source: PathBuf,
    version: String,
    items: Vec<Item>,
}

static CATALOG: OnceLock<Result<Catalog, String>> = OnceLock::new();

/// Подсказки из официальной локальной справки платформы.
pub fn suggest(task: &str, source: &str, cursor: Option<&Value>, limit: usize) -> Value {
    let catalog = match CATALOG.get_or_init(load_catalog) {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "available": false,
                "items": [],
                "completions": [],
                "reason": e,
            })
        }
    };

    let terms = stems(task);
    let task_low = task.to_lowercase();
    let has_exact_title = catalog.items.iter().any(|item| {
        let title = item.title.to_lowercase();
        task_low.contains(&title) || title.contains(&task_low)
    });
    let owners: HashSet<String> = catalog
        .items
        .iter()
        .map(|item| item.owner.clone())
        .collect();
    let target_owners = select_target_owners(owners.iter().map(String::as_str), task, &terms);
    let mut ranked: Vec<(usize, &Item)> = catalog
        .items
        .iter()
        .filter_map(|item| {
            if !target_owners.is_empty() && !target_owners.contains(&item.owner.to_lowercase()) {
                return None;
            }
            let title = item.title.to_lowercase();
            let exact = task_low.contains(&title) || title.contains(&task_low);
            if has_exact_title && !exact {
                return None;
            }
            let content = format!(
                "{} {} {}",
                item.name.to_lowercase(),
                item.description.to_lowercase(),
                item.syntax.to_lowercase()
            );
            let hay = format!("{} {}", title, content);
            let matched = terms.iter().filter(|t| hay.contains(t.as_str())).count();
            let owner_terms = camel_stems(&item.owner);
            let intent_terms: Vec<_> = terms.iter().filter(|t| !owner_terms.contains(t)).collect();
            let intent_matched = intent_terms
                .iter()
                .filter(|t| content.contains(t.as_str()))
                .count();
            let intent_required = intent_terms.len().min(MIN_TERM_MATCHES);
            if !exact
                && (matched < MIN_TERM_MATCHES
                    || (!target_owners.is_empty() && intent_matched < intent_required))
            {
                return None;
            }
            let title_hits = terms.iter().filter(|t| title.contains(t.as_str())).count();
            // Короткое имя типа, названное пользователем, важнее длинного
            // составного типа, случайно вобравшего те же слова. Например,
            // «добавить элемент в структуру» должно вести к `Структура`, а
            // не к `КоллекцияЭлементовСтруктурыДиаграммыКомпоновкиДанных`.
            let exact_owner = target_owners.contains(&item.owner.to_lowercase());
            let length_penalty = item.title.chars().count().min(200);
            Some((
                usize::from(exact) * 10_000
                    + usize::from(exact_owner) * 2_000
                    + matched * 100
                    + title_hits * 50
                    - length_penalty,
                item,
            ))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.title.cmp(&b.1.title)));
    if !target_owners.is_empty() {
        let best = ranked.first().map(|(score, _)| *score).unwrap_or(0);
        ranked.retain(|(score, _)| score.saturating_add(TARGET_SCORE_WINDOW) >= best);
    }

    let mut seen = HashSet::new();
    let items: Vec<Value> = ranked
        .into_iter()
        .filter(|(_, i)| seen.insert(i.title.to_lowercase()))
        .take(limit)
        .map(|(score, i)| {
            json!({
                "title": i.title,
                "kind": i.kind,
                "syntax": i.syntax,
                "description": i.description,
                "availability": i.availability,
                "example": i.example,
                "score": score,
                "source_page": i.path,
            })
        })
        .collect();

    let prefix = cursor_prefix(source, cursor);
    let completions: Vec<Value> = prefix
        .as_deref()
        .and_then(|p| p.rsplit_once('.').map(|(owner, name)| (p, owner, name)))
        .map(|(whole, owner, name)| {
            let owner = owner.to_lowercase();
            let name = name.to_lowercase();
            catalog
                .items
                .iter()
                .filter(|i| {
                    i.owner.to_lowercase() == owner && i.name.to_lowercase().starts_with(&name)
                })
                .take(limit)
                .map(|i| {
                    json!({
                        "replace": whole,
                        "with": i.title,
                        "syntax": i.syntax,
                        "kind": i.kind,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let methods = catalog.items.iter().filter(|i| i.kind == "method").count();
    let properties = catalog
        .items
        .iter()
        .filter(|i| i.kind == "property")
        .count();
    let constructors = catalog
        .items
        .iter()
        .filter(|i| i.kind == "constructor")
        .count();
    let events = catalog.items.iter().filter(|i| i.kind == "event").count();
    let mut target_owner_list: Vec<&str> = target_owners.iter().map(String::as_str).collect();
    target_owner_list.sort();
    json!({
        "available": true,
        "version": catalog.version,
        "source": catalog.source,
        "target_owners": target_owner_list,
        "catalog_items": catalog.items.len(),
        "catalog_by_kind": {
            "methods": methods,
            "properties": properties,
            "constructors": constructors,
            "events": events,
        },
        "items": items,
        "completions": completions,
        "no_reliable_suggestions": items.is_empty() && completions.is_empty(),
        "note": "Источник — локальный официальный shcntx_ru.hbk. Возвращаются только точное имя либо совпадение минимум по двум значимым основам слов; слабые результаты скрыты."
    })
}

fn select_target_owners<'a>(
    owners: impl Iterator<Item = &'a str>,
    task: &str,
    terms: &[String],
) -> HashSet<String> {
    let owners: Vec<&str> = owners.collect();
    let task_compact = compact_name(task);
    let exact: Vec<(usize, String)> = if task.contains('.') {
        owners
            .iter()
            .filter_map(|owner| {
                let compact = compact_name(owner);
                (compact.chars().count() >= 4 && task_compact.contains(&compact))
                    .then(|| (compact.chars().count(), owner.to_lowercase()))
            })
            .collect()
    } else {
        Vec::new()
    };
    if let Some(longest) = exact.iter().map(|(length, _)| *length).max() {
        return exact
            .into_iter()
            .filter_map(|(length, owner)| (length == longest).then_some(owner))
            .collect();
    }

    // В вопросе о событии слова «поле» и «форма» описывают контекст, но не
    // обязательно точный тип-владелец. Сужение по ним прячет специализации.
    if terms.iter().any(|term| term == "событ") {
        return HashSet::new();
    }

    let fuzzy: Vec<(usize, usize, String)> = owners
        .iter()
        .filter_map(|owner| {
            let owner_terms = camel_stems(owner);
            if owner_terms.is_empty() || !owner_terms.iter().all(|term| terms.contains(term)) {
                return None;
            }
            let last_position = owner_terms
                .iter()
                .filter_map(|owner_term| terms.iter().position(|term| term == owner_term))
                .max()?;
            Some((owner_terms.len(), last_position, owner.to_lowercase()))
        })
        .collect();
    let shortest = fuzzy.iter().map(|(count, _, _)| *count).min();
    let latest = fuzzy
        .iter()
        .filter(|(count, _, _)| Some(*count) == shortest)
        .map(|(_, position, _)| *position)
        .max();
    fuzzy
        .into_iter()
        .filter_map(|(count, position, owner)| {
            (Some(count) == shortest && Some(position) == latest).then_some(owner)
        })
        .collect()
}

fn load_catalog() -> Result<Catalog, String> {
    let source = locate_hbk().ok_or(
        "не найден shcntx_ru.hbk; задайте GYRFALCON_PLATFORM_BIN каталогом bin установленной платформы 1С",
    )?;
    let version = source
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let bytes =
        std::fs::read(&source).map_err(|e| format!("не прочитан {}: {e}", source.display()))?;
    let storage = extract_entity(&bytes, "FileStorage")?;
    let mut zip = zip::ZipArchive::new(Cursor::new(storage))
        .map_err(|e| format!("FileStorage в {} не является ZIP: {e}", source.display()))?;
    let mut items = Vec::new();
    for index in 0..zip.len() {
        let mut file = zip.by_index(index).map_err(|e| e.to_string())?;
        let path = file.name().replace('\\', "/");
        if !path.ends_with(".html") || !interesting_path(&path) {
            continue;
        }
        let mut html = String::new();
        file.read_to_string(&mut html)
            .map_err(|e| format!("не прочитана страница {path}: {e}"))?;
        if let Some(item) = parse_page(&path, &html) {
            items.push(item);
        }
    }
    if items.is_empty() {
        return Err(format!(
            "в {} не найдено ни одной страницы API",
            source.display()
        ));
    }
    Ok(Catalog {
        source,
        version,
        items,
    })
}

fn interesting_path(path: &str) -> bool {
    path.contains("/methods/")
        || path.contains("/properties/")
        || path.contains("/ctors/")
        || path.contains("/events/")
}

fn parse_page(path: &str, html: &str) -> Option<Item> {
    let title = tag_text(html, "<h1 class=\"V8SH_pagetitle\">")?;
    let russian_title = title
        .split_once(" (")
        .map(|(ru, _)| ru)
        .unwrap_or(&title)
        .trim()
        .to_string();
    let (owner, name) = russian_title
        .rsplit_once('.')
        .map(|(o, n)| (o.to_string(), n.to_string()))
        .unwrap_or_else(|| (String::new(), russian_title.clone()));
    let kind = if path.contains("/methods/") {
        "method"
    } else if path.contains("/properties/") {
        "property"
    } else if path.contains("/ctors/") {
        "constructor"
    } else {
        "event"
    };
    Some(Item {
        title: russian_title,
        owner,
        name,
        kind: kind.to_string(),
        syntax: section(html, "Синтаксис"),
        description: section(html, "Описание"),
        availability: section(html, "Доступность"),
        example: section(html, "Пример"),
        path: path.to_string(),
    })
}

fn tag_text(html: &str, marker: &str) -> Option<String> {
    let tail = html.split_once(marker)?.1;
    let raw = tail.split_once("</h1>")?.0;
    Some(clean_html(raw))
}

fn section(html: &str, name: &str) -> String {
    let marker = "<p class=\"V8SH_chapter\">";
    for block in html.split(marker).skip(1) {
        let Some((heading, tail)) = block.split_once("</p>") else {
            continue;
        };
        if clean_html(heading).trim().trim_end_matches(':').trim() != name {
            continue;
        }
        let raw = tail.split(marker).next().unwrap_or(tail);
        return clean_html(raw).chars().take(MAX_SECTION).collect();
    }
    String::new()
}

fn clean_html(raw: &str) -> String {
    static BREAKS: OnceLock<Regex> = OnceLock::new();
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();
    let breaks = BREAKS.get_or_init(|| Regex::new("(?i)<br\\s*/?>|</p>|</li>|</tr>").unwrap());
    let tags = TAGS.get_or_init(|| Regex::new("(?s)<[^>]+>").unwrap());
    let spaces = SPACES.get_or_init(|| Regex::new(r"[ \t\r\n]+").unwrap());
    let with_breaks = breaks.replace_all(raw, "\n");
    let no_tags = tags.replace_all(&with_breaks, "");
    let decoded = no_tags
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&amp;", "&");
    // Отдельные строки справки не нужны: компактная выдача полезнее верстки.
    spaces.replace_all(decoded.trim(), " ").to_string()
}

fn extract_entity(bytes: &[u8], wanted: &str) -> Result<Vec<u8>, String> {
    let mut pos = 18usize;
    let payload = hex_len(bytes, &mut pos)?;
    let block = hex_len(bytes, &mut pos)?;
    pos = pos.checked_add(11).ok_or("переполнение позиции HBK")?;
    let end = pos.checked_add(payload).ok_or("переполнение таблицы HBK")?;
    let infos = bytes.get(pos..end).ok_or("обрезана таблица файлов HBK")?;
    // `block` читается для проверки заголовка: таблица должна умещаться в блок.
    if block < payload {
        return Err("некорректный HBK: блок таблицы меньше полезных данных".into());
    }
    for info in infos.chunks_exact(12) {
        let header = u32::from_le_bytes(info[0..4].try_into().unwrap()) as usize;
        let body = u32::from_le_bytes(info[4..8].try_into().unwrap()) as usize;
        let reserved = u32::from_le_bytes(info[8..12].try_into().unwrap());
        if reserved != i32::MAX as u32 {
            continue;
        }
        let mut hp = header.checked_add(2).ok_or("переполнение адреса HBK")?;
        let name_payload = hex_len(bytes, &mut hp)?;
        hp = hp.checked_add(40).ok_or("переполнение заголовка HBK")?;
        let name_len = name_payload
            .checked_sub(24)
            .ok_or("некорректная длина имени HBK")?;
        let name_bytes = bytes
            .get(hp..hp + name_len)
            .ok_or("обрезано имя файла HBK")?;
        let utf16: Vec<u16> = name_bytes
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        let name = String::from_utf16_lossy(&utf16);
        if name != wanted {
            continue;
        }
        let mut bp = body.checked_add(2).ok_or("переполнение тела HBK")?;
        let body_len = hex_len(bytes, &mut bp)?;
        bp = bp
            .checked_add(20)
            .ok_or("переполнение заголовка тела HBK")?;
        return bytes
            .get(bp..bp + body_len)
            .map(|b| b.to_vec())
            .ok_or_else(|| "обрезано тело FileStorage в HBK".to_string());
    }
    Err(format!("в HBK не найден {wanted}"))
}

fn hex_len(bytes: &[u8], pos: &mut usize) -> Result<usize, String> {
    let text = bytes
        .get(*pos..*pos + 8)
        .ok_or("обрезано шестнадцатеричное поле HBK")?;
    *pos += 9; // восемь цифр и разделитель
    let text = std::str::from_utf8(text).map_err(|_| "длина HBK не ASCII")?;
    usize::from_str_radix(text, 16).map_err(|_| format!("неверная длина HBK: {text}"))
}

fn locate_hbk() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("GYRFALCON_PLATFORM_BIN") {
        let path = PathBuf::from(path);
        let file = if path.is_file() {
            path
        } else {
            path.join("shcntx_ru.hbk")
        };
        if file.is_file() {
            return Some(file);
        }
    }

    let mut candidates = Vec::new();
    for env_name in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Some(base) = std::env::var_os(env_name) else {
            continue;
        };
        let root = PathBuf::from(base).join("1cv8");
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path().join("bin").join("shcntx_ru.hbk");
            if path.is_file() {
                candidates.push(path);
            }
        }
    }
    candidates.sort_by_key(|p| {
        p.parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .map(|s| version_key(&s.to_string_lossy()))
            .unwrap_or_default()
    });
    candidates.pop()
}

fn version_key(version: &str) -> Vec<u32> {
    version
        .split('.')
        .map(|p| p.parse::<u32>().unwrap_or(0))
        .collect()
}

fn stems(text: &str) -> Vec<String> {
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
        "получить",
    ];
    let mut out = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let lower = word.to_lowercase();
        if lower.chars().count() < 4 || STOP.contains(&lower.as_str()) {
            continue;
        }
        let has_internal_upper = word.chars().skip(1).any(char::is_uppercase);
        let word_stems = if has_internal_upper || word.contains('_') {
            camel_stems(word)
        } else {
            vec![lower.chars().take(5).collect()]
        };
        for stem in word_stems {
            if !out.contains(&stem) {
                out.push(stem);
            }
        }
    }
    out
}

fn compact_name(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn camel_stems(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (index, ch) in chars.iter().copied().enumerate() {
        if !ch.is_alphanumeric() && ch != '_' {
            if !current.is_empty() {
                words.push(current);
                current = String::new();
            }
            continue;
        }
        let prev = index.checked_sub(1).and_then(|i| chars.get(i)).copied();
        let next = chars.get(index + 1).copied();
        let boundary = ch.is_uppercase()
            && !current.is_empty()
            && (prev.is_some_and(char::is_lowercase)
                || (prev.is_some_and(char::is_uppercase) && next.is_some_and(char::is_lowercase)));
        if boundary {
            words.push(current);
            current = String::new();
        }
        current.extend(ch.to_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
        .into_iter()
        // Короткие акронимы тоже являются частью имени владельца. Если
        // отбросить `DOM`, тип `ЭлементDOM` ложно выглядит как `Элемент`.
        .filter(|w| w.chars().count() >= 3)
        .map(|w| w.chars().take(5).collect())
        .collect()
}

fn cursor_prefix(source: &str, cursor: Option<&Value>) -> Option<String> {
    if source.is_empty() {
        return None;
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
    (prefix.chars().count() >= 2).then_some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_platform_page_without_html_noise() {
        let html = r#"<h1 class="V8SH_pagetitle">Структура.Вставить (Structure.Insert)</h1>
          <p class="V8SH_chapter">Синтаксис:</p>Вставить(&lt;Ключ&gt;, &lt;Значение&gt;)
          <p class="V8SH_chapter">Описание:</p><p>Добавляет <b>элемент</b>.</p>
          <p class="V8SH_chapter">Доступность:</p>Сервер.<BR>Тонкий клиент.
          <p class="V8SH_chapter">Пример:</p>С.Вставить("А", 1);"#;
        let item = parse_page("objects/Structure/methods/Insert.html", html).unwrap();
        assert_eq!(item.title, "Структура.Вставить");
        assert_eq!(item.owner, "Структура");
        assert_eq!(item.name, "Вставить");
        assert_eq!(item.syntax, "Вставить(<Ключ>, <Значение>)");
        assert_eq!(item.description, "Добавляет элемент.");
        assert!(!item.availability.contains('<'));
    }

    #[test]
    fn camel_stems_do_not_hide_unmatched_acronyms() {
        assert_eq!(camel_stems("Структура"), vec!["струк"]);
        assert_eq!(camel_stems("ЭлементDOM"), vec!["элеме", "dom"]);
        assert_eq!(camel_stems("Табличная часть"), vec!["табли", "часть"]);
        assert_eq!(
            camel_stems("КоллекцияЭлементовСтруктуры"),
            vec!["колле", "элеме", "струк"]
        );
        assert_eq!(
            stems("КоллекцияЭлементовСтруктуры.Добавить"),
            vec!["колле", "элеме", "струк", "добав"]
        );
        assert_eq!(compact_name("Таблица значений"), "таблицазначений");
    }

    #[test]
    fn natural_task_selects_short_owner_at_the_last_noun() {
        let owners = [
            "Элемент",
            "ЭлементDOM",
            "Структура",
            "КоллекцияЭлементовСтруктурыТаблицыКомпоновкиДанных",
        ];
        let task = "как добавить элемент в структуру";
        let selected = select_target_owners(owners.iter().copied(), task, &stems(task));
        assert_eq!(selected, HashSet::from(["структура".to_string()]));
    }

    #[test]
    fn exact_long_owner_wins_over_nested_short_owner() {
        let owners = [
            "Структура",
            "КоллекцияЭлементовСтруктурыДиаграммыКомпоновкиДанных",
        ];
        let task = "КоллекцияЭлементовСтруктурыДиаграммыКомпоновкиДанных.Добавить";
        let selected = select_target_owners(owners.iter().copied(), task, &stems(task));
        assert_eq!(
            selected,
            HashSet::from(["коллекцияэлементовструктурыдиаграммыкомпоновкиданных".to_string()])
        );
    }

    #[test]
    fn version_sort_is_numeric() {
        assert!(version_key("8.3.27.2130") > version_key("8.3.9.9999"));
    }

    #[test]
    fn cursor_is_unicode_offset() {
        let s = "Структура.Вст\nКонец";
        let at = "Структура.Вст".chars().count();
        assert_eq!(cursor_prefix(s, Some(&json!(at))).unwrap(), "Структура.Вст");
    }
}
