//! Пользовательская встроенная справка Ext/Help в том же индексе, что и проект.
//! Источник истины — подключённые исходники, не отдельная общая KB.

use quick_xml::events::Event;
use quick_xml::Reader;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use walkdir::WalkDir;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS help_sources(
 source_id TEXT PRIMARY KEY, root TEXT NOT NULL, config_version TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS help_pages(
 id INTEGER PRIMARY KEY, source_id TEXT NOT NULL REFERENCES help_sources(source_id),
 path TEXT NOT NULL, category TEXT NOT NULL, object_name TEXT NOT NULL,
 form_name TEXT NOT NULL, title TEXT NOT NULL, text TEXT NOT NULL,
 chars INTEGER NOT NULL, sha256 TEXT NOT NULL,
 UNIQUE(source_id,path)
);
CREATE INDEX IF NOT EXISTS ix_help_source ON help_pages(source_id);
CREATE TABLE IF NOT EXISTS help_files(
 source_id TEXT NOT NULL REFERENCES help_sources(source_id),
 path TEXT NOT NULL, raw_sha256 TEXT NOT NULL,
 indexed INTEGER NOT NULL,
 PRIMARY KEY(source_id,path)
);
CREATE VIRTUAL TABLE IF NOT EXISTS help_fts USING fts5(
 title, object_name, text, content='help_pages', content_rowid='id',
 tokenize='unicode61 remove_diacritics 2'
);
"#;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HelpReport {
    pub html_files: usize,
    pub raw_html_bytes: usize,
    /// Прочитано и захэшировано; HTML разбирается только при изменении хэша.
    pub hashed: usize,
    pub parsed: usize,
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub skipped_short: usize,
    pub unreadable: usize,
    pub title_only: usize,
    pub duplicate_texts: usize,
    pub text_bytes: usize,
}

#[derive(Debug)]
struct Page {
    path: String,
    category: String,
    object: String,
    form: String,
    title: String,
    text: String,
    sha: String,
}

struct FileRecord {
    path: String,
    raw_sha: String,
    page: Option<Page>,
    unchanged: bool,
}

#[derive(Default)]
pub struct Snapshot {
    sources: Vec<(String, String, String)>,
    files: Vec<(String, String, String, i64)>,
    pages: Vec<SnapshotPage>,
}

struct SnapshotPage {
    id: i64,
    source_id: String,
    path: String,
    category: String,
    object_name: String,
    form_name: String,
    title: String,
    text: String,
    chars: i64,
    sha256: String,
}

impl Snapshot {
    pub fn primary_root(&self) -> Option<&str> {
        self.sources
            .iter()
            .find(|(id, _, _)| id == "primary")
            .map(|(_, root, _)| root.as_str())
    }

    pub fn extra_sources(&self) -> Vec<(String, PathBuf)> {
        self.sources
            .iter()
            .filter(|(id, _, _)| id != "primary")
            .map(|(id, root, _)| (id.clone(), PathBuf::from(root)))
            .collect()
    }
}

/// Транзакционно синхронизировать один подключённый источник.
/// Удаляются только его исчезнувшие страницы. Ошибка чтения отменяет sync,
/// чтобы временно недоступные исходники не выглядели удалёнными.
pub fn sync(conn: &mut Connection, source_id: &str, root: &Path) -> Result<HelpReport, String> {
    if source_id.trim().is_empty() || !root.is_dir() {
        return Err("нужны непустой source_id и существующий каталог исходников".into());
    }
    let canonical = root
        .canonicalize()
        .map_err(|e| format!("не удалось разрешить {}: {e}", root.display()))?;
    let version = config_version(&canonical)?;
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    let previous_source: Option<(String, String)> = conn
        .query_row(
            "SELECT root,config_version FROM help_sources WHERE source_id=?1",
            [source_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some((previous, _)) = &previous_source {
        if Path::new(previous) != canonical {
            return Err(format!(
                "source_id '{source_id}' уже закреплён за {previous}; другой путь не заменит источник молча"
            ));
        }
    }
    let previous_hashes: HashMap<String, (String, bool)> = {
        let mut stmt = conn
            .prepare("SELECT path,raw_sha256,indexed FROM help_files WHERE source_id=?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([source_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get(1)?, r.get::<_, i64>(2)? != 0),
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };
    let (files, mut report) = collect(&canonical, &previous_hashes)?;
    if report.unreadable > 0 {
        return Err(format!(
            "{} HTML-страниц не прочитано; источник не обновлён, чтобы не потерять справку",
            report.unreadable
        ));
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    if !previous_source
        .as_ref()
        .is_some_and(|(_, old_version)| *old_version == version)
    {
        tx.execute(
            "INSERT INTO help_sources(source_id,root,config_version) VALUES(?1,?2,?3)
             ON CONFLICT(source_id) DO UPDATE SET root=excluded.root,config_version=excluded.config_version",
            params![source_id, canonical.to_string_lossy(), version],
        )
        .map_err(|e| e.to_string())?;
    }
    let mut seen = HashSet::new();
    for file in files {
        seen.insert(file.path.clone());
        if file.unchanged {
            continue;
        }
        let Some(page) = file.page else {
            if delete_page(&tx, source_id, &file.path)? {
                report.removed += 1;
            }
            tx.execute(
                "INSERT INTO help_files(source_id,path,raw_sha256,indexed) VALUES(?1,?2,?3,0)
                 ON CONFLICT(source_id,path) DO UPDATE SET raw_sha256=excluded.raw_sha256,indexed=0",
                params![source_id, file.path, file.raw_sha],
            )
            .map_err(|e| e.to_string())?;
            continue;
        };
        let old: Option<(i64, String)> = tx
            .query_row(
                "SELECT id,sha256 FROM help_pages WHERE source_id=?1 AND path=?2",
                params![source_id, page.path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some((id, old_hash)) = old {
            if old_hash == page.sha {
                report.unchanged += 1;
                tx.execute(
                    "INSERT INTO help_files(source_id,path,raw_sha256,indexed) VALUES(?1,?2,?3,1)
                     ON CONFLICT(source_id,path) DO UPDATE SET raw_sha256=excluded.raw_sha256,indexed=1",
                    params![source_id, file.path, file.raw_sha],
                )
                .map_err(|e| e.to_string())?;
                continue;
            }
            tx.execute(
                "INSERT INTO help_fts(help_fts,rowid,title,object_name,text)
                 SELECT 'delete',id,title,object_name,text FROM help_pages WHERE id=?1",
                [id],
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
                "UPDATE help_pages SET category=?2,object_name=?3,form_name=?4,title=?5,
                 text=?6,chars=?7,sha256=?8 WHERE id=?1",
                params![
                    id,
                    page.category,
                    page.object,
                    page.form,
                    page.title,
                    page.text,
                    page.text.chars().count() as i64,
                    page.sha
                ],
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO help_fts(rowid,title,object_name,text) VALUES(?1,?2,?3,?4)",
                params![id, page.title, page.object, page.text],
            )
            .map_err(|e| e.to_string())?;
            report.updated += 1;
        } else {
            tx.execute(
                "INSERT INTO help_pages(source_id,path,category,object_name,form_name,title,text,chars,sha256)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![source_id, page.path, page.category, page.object, page.form,
                        page.title, page.text, page.text.chars().count() as i64, page.sha],
            )
            .map_err(|e| e.to_string())?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO help_fts(rowid,title,object_name,text) VALUES(?1,?2,?3,?4)",
                params![id, page.title, page.object, page.text],
            )
            .map_err(|e| e.to_string())?;
            report.added += 1;
        }
        tx.execute(
            "INSERT INTO help_files(source_id,path,raw_sha256,indexed) VALUES(?1,?2,?3,1)
             ON CONFLICT(source_id,path) DO UPDATE SET raw_sha256=excluded.raw_sha256,indexed=1",
            params![source_id, file.path, file.raw_sha],
        )
        .map_err(|e| e.to_string())?;
    }
    let old: Vec<(i64, String)> = {
        let mut stmt = tx
            .prepare("SELECT id,path FROM help_pages WHERE source_id=?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([source_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };
    for (id, path) in old {
        if seen.contains(&path) {
            continue;
        }
        tx.execute(
            "INSERT INTO help_fts(help_fts,rowid,title,object_name,text)
             SELECT 'delete',id,title,object_name,text FROM help_pages WHERE id=?1",
            [id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM help_pages WHERE id=?1", [id])
            .map_err(|e| e.to_string())?;
        report.removed += 1;
    }
    for path in previous_hashes.keys() {
        if !seen.contains(path) {
            tx.execute(
                "DELETE FROM help_files WHERE source_id=?1 AND path=?2",
                params![source_id, path],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(report)
}

/// Отключить один источник внутри одного индекса; остальные не затрагиваются.
pub fn remove(conn: &mut Connection, source_id: &str) -> Result<usize, String> {
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let rows: Vec<i64> = {
        let mut stmt = tx
            .prepare("SELECT id FROM help_pages WHERE source_id=?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([source_id], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };
    for id in &rows {
        tx.execute(
            "INSERT INTO help_fts(help_fts,rowid,title,object_name,text)
             SELECT 'delete',id,title,object_name,text FROM help_pages WHERE id=?1",
            [id],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.execute("DELETE FROM help_pages WHERE source_id=?1", [source_id])
        .map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM help_files WHERE source_id=?1", [source_id])
        .map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM help_sources WHERE source_id=?1", [source_id])
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(rows.len())
}

fn delete_page(
    tx: &rusqlite::Transaction<'_>,
    source_id: &str,
    path: &str,
) -> Result<bool, String> {
    let id: Option<i64> = tx
        .query_row(
            "SELECT id FROM help_pages WHERE source_id=?1 AND path=?2",
            params![source_id, path],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some(id) = id else { return Ok(false) };
    tx.execute(
        "INSERT INTO help_fts(help_fts,rowid,title,object_name,text)
         SELECT 'delete',id,title,object_name,text FROM help_pages WHERE id=?1",
        [id],
    )
    .map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM help_pages WHERE id=?1", [id])
        .map_err(|e| e.to_string())?;
    Ok(true)
}

fn collect(
    root: &Path,
    previous: &HashMap<String, (String, bool)>,
) -> Result<(Vec<FileRecord>, HelpReport), String> {
    let mut files = Vec::new();
    let mut report = HelpReport::default();
    let mut hashes = HashSet::new();
    for entry in WalkDir::new(root).into_iter() {
        let entry = entry.map_err(|e| format!("ошибка обхода Ext/Help: {e}"))?;
        if !entry.file_type().is_file() || !is_help_html(entry.path()) {
            continue;
        }
        report.html_files += 1;
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let raw = match std::fs::read(entry.path()) {
            Ok(v) => v,
            Err(_) => {
                report.unreadable += 1;
                continue;
            }
        };
        report.raw_html_bytes += raw.len();
        report.hashed += 1;
        let raw_sha = format!("{:x}", Sha256::digest(&raw));
        if let Some((old_sha, indexed)) = previous.get(&rel) {
            if *old_sha == raw_sha {
                if *indexed {
                    report.unchanged += 1;
                } else {
                    report.skipped_short += 1;
                }
                files.push(FileRecord {
                    path: rel,
                    raw_sha,
                    page: None,
                    unchanged: true,
                });
                continue;
            }
        }
        report.parsed += 1;
        let raw = raw.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&raw);
        let html = match std::str::from_utf8(raw) {
            Ok(v) => v,
            Err(_) => {
                report.unreadable += 1;
                continue;
            }
        };
        let text = html_to_text(html);
        if text.contains('\0')
            || text.contains('\u{fffd}')
            || text.to_ascii_lowercase().contains("data:image")
            || text.to_ascii_lowercase().contains("base64,")
        {
            return Err(format!(
                "в извлечённом тексте есть бинарный или медиа-payload: {rel}"
            ));
        }
        if text.chars().count() < 40 {
            report.skipped_short += 1;
            files.push(FileRecord {
                path: rel,
                raw_sha,
                page: None,
                unchanged: false,
            });
            continue;
        }
        let title: String = text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        if text == title {
            report.title_only += 1;
        }
        let sha = format!("{:x}", Sha256::digest(text.as_bytes()));
        if !hashes.insert(sha.clone()) {
            report.duplicate_texts += 1;
        }
        report.text_bytes += text.len();
        let parts: Vec<&str> = rel.split('/').collect();
        let page = Page {
            path: rel.clone(),
            category: parts.first().unwrap_or(&"").to_string(),
            object: parts.get(1).unwrap_or(&"").to_string(),
            form: if parts.get(2) == Some(&"Forms") {
                parts.get(3).unwrap_or(&"").to_string()
            } else {
                String::new()
            },
            title,
            text,
            sha,
        };
        files.push(FileRecord {
            path: rel,
            raw_sha,
            page: Some(page),
            unchanged: false,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    if report.html_files != files.len() + report.unreadable {
        return Err("контроль покрытия Ext/Help не сошёлся".into());
    }
    Ok((files, report))
}

fn is_help_html(path: &Path) -> bool {
    let parts: Vec<_> = path.components().collect();
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("html"))
        && parts.windows(2).any(|w| {
            w[0].as_os_str().eq_ignore_ascii_case("Ext")
                && w[1].as_os_str().eq_ignore_ascii_case("Help")
        })
}

fn html_to_text(raw: &str) -> String {
    static SCRIPT: OnceLock<Regex> = OnceLock::new();
    static BREAK: OnceLock<Regex> = OnceLock::new();
    static TAG: OnceLock<Regex> = OnceLock::new();
    static SPACE: OnceLock<Regex> = OnceLock::new();
    static NEWLINES: OnceLock<Regex> = OnceLock::new();
    let script = SCRIPT
        .get_or_init(|| Regex::new(r"(?is)<(script|style)[^>]*>.*?</(?:script|style)>").unwrap());
    let br = BREAK.get_or_init(|| Regex::new(r"(?i)<br\s*/?>|</(?:p|div|li|tr|h[1-6])>").unwrap());
    let tag = TAG.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    let space = SPACE.get_or_init(|| Regex::new(r"[ \t\u{00a0}]+").unwrap());
    let newlines = NEWLINES.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    let no_script = script.replace_all(raw, " ");
    let with_lines = br.replace_all(&no_script, "\n");
    let no_tags = tag.replace_all(&with_lines, " ");
    let decoded = decode_entities(&no_tags);
    let compact = space.replace_all(&decoded, " ");
    let lines = compact
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n");
    newlines.replace_all(lines.trim(), "\n\n").into_owned()
}

fn decode_entities(s: &str) -> String {
    let mut out = s.to_string();
    for (from, to) in [
        ("&nbsp;", " "),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
    ] {
        out = out.replace(from, to);
    }
    out
}

fn config_version(root: &Path) -> Result<String, String> {
    let path = root.join("Configuration.xml");
    if !path.is_file() {
        return Ok(String::new());
    }
    let mut reader = Reader::from_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = Vec::new();
    let mut in_properties = false;
    let mut in_version = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name();
                if name.as_ref() == b"Properties" {
                    in_properties = true;
                } else if in_properties && name.as_ref() == b"Version" {
                    in_version = true;
                }
            }
            Ok(Event::Text(e)) if in_version => {
                return e
                    .unescape()
                    .map(|v| v.into_owned())
                    .map_err(|e| e.to_string());
            }
            Ok(Event::End(e)) => {
                let name = e.local_name();
                if name.as_ref() == b"Version" {
                    in_version = false;
                }
                if name.as_ref() == b"Properties" {
                    in_properties = false;
                }
            }
            Ok(Event::Eof) => return Ok(String::new()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
            _ => {}
        }
        buf.clear();
    }
}

pub fn source_roots(conn: &Connection) -> Result<Vec<(String, PathBuf)>, String> {
    let mut stmt = conn
        .prepare("SELECT source_id,root FROM help_sources ORDER BY source_id")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?)))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string());
    rows
}

/// Запомнить подключённые дополнительные источники до полной пересборки:
/// build создаёт новый файл индекса, поэтому одна только старая таблица их не сохранит.
pub fn registered_sources(index: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    if !index.is_file() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(index, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    let exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='help_sources'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if exists == 0 {
        return Ok(Vec::new());
    }
    source_roots(&conn)
}

/// Сохранить только раздел Help перед полной пересборкой кода. Даже большой
/// проект держит здесь десятки мегабайт текста, а не гигабайты кода индекса.
pub fn snapshot(index: &Path) -> Result<Snapshot, String> {
    if !index.is_file() {
        return Ok(Snapshot::default());
    }
    let conn = Connection::open_with_flags(index, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    let exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='help_sources'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if exists == 0 {
        return Ok(Snapshot::default());
    }
    let mut result = Snapshot::default();
    {
        let mut stmt = conn
            .prepare("SELECT source_id,root,config_version FROM help_sources")
            .map_err(|e| e.to_string())?;
        result.sources = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
    }
    {
        let mut stmt = conn.prepare(
            "SELECT id,source_id,path,category,object_name,form_name,title,text,chars,sha256 FROM help_pages"
        ).map_err(|e| e.to_string())?;
        result.pages = stmt
            .query_map([], |r| {
                Ok(SnapshotPage {
                    id: r.get(0)?,
                    source_id: r.get(1)?,
                    path: r.get(2)?,
                    category: r.get(3)?,
                    object_name: r.get(4)?,
                    form_name: r.get(5)?,
                    title: r.get(6)?,
                    text: r.get(7)?,
                    chars: r.get(8)?,
                    sha256: r.get(9)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
    }
    let files_exist: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='help_files'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if files_exist != 0 {
        let mut stmt = conn
            .prepare("SELECT source_id,path,raw_sha256,indexed FROM help_files")
            .map_err(|e| e.to_string())?;
        result.files = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
    }
    Ok(result)
}

/// Восстановить страницы с теми же id и хэшами; следующий sync меняет лишь
/// действительно изменившиеся HTML и удаляет исчезнувшие.
pub fn restore(conn: &mut Connection, snapshot: Snapshot) -> Result<(), String> {
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    for (id, root, version) in snapshot.sources {
        tx.execute(
            "INSERT INTO help_sources VALUES(?1,?2,?3)",
            params![id, root, version],
        )
        .map_err(|e| e.to_string())?;
    }
    for (source_id, path, sha, indexed) in snapshot.files {
        tx.execute(
            "INSERT INTO help_files VALUES(?1,?2,?3,?4)",
            params![source_id, path, sha, indexed],
        )
        .map_err(|e| e.to_string())?;
    }
    for page in snapshot.pages {
        tx.execute(
            "INSERT INTO help_pages(id,source_id,path,category,object_name,form_name,title,text,chars,sha256)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![page.id, page.source_id, page.path, page.category, page.object_name,
                page.form_name, page.title, page.text, page.chars, page.sha256],
        ).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO help_fts(rowid,title,object_name,text) VALUES(?1,?2,?3,?4)",
            params![page.id, page.title, page.object_name, page.text],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn sandbox() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("gyrfalcon-help-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn page(root: &Path, object: &str, body: &str) -> PathBuf {
        let path = root.join(format!("Documents/{object}/Ext/Help/ru.html"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn чистое_извлечение_не_несёт_медиа_скрипты_и_служебные_файлы() {
        let root = sandbox();
        page(&root, "Заказ", "<html><style>.bad{color:red}</style><h1>Заказ клиента</h1><p>Оформите заказ клиента и заполните необходимые поля.</p><img src=\"data:image/png;base64,AAAA\"><script>alert('bad')</script></html>");
        std::fs::write(
            root.join("Documents/Заказ/Ext/Help/picture.png"),
            [0, 255, 1],
        )
        .unwrap();
        let (pages, report) = collect(&root, &HashMap::new()).unwrap();
        assert_eq!(report.html_files, 1);
        assert_eq!(pages.len(), 1);
        assert_eq!(
            pages[0].page.as_ref().unwrap().text,
            "Заказ клиента\nОформите заказ клиента и заполните необходимые поля."
        );
        assert!(!pages[0].page.as_ref().unwrap().text.contains("base64"));
        assert!(!pages[0].page.as_ref().unwrap().text.contains("alert"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn два_проекта_и_два_источника_обновляются_изолированно() {
        let root = sandbox();
        let a = root.join("a");
        let a_ext = root.join("a-ext");
        let b = root.join("b");
        let a_page = page(
            &a,
            "Заказ",
            "<h1>Заказ клиента</h1><p>Оформление продажи клиенту через документ заказ.</p>",
        );
        page(
            &a_ext,
            "Дополнение",
            "<h1>Дополнение</h1><p>Уникальная инструкция дополнительного источника.</p>",
        );
        page(
            &b,
            "Отпуск",
            "<h1>Отпуск</h1><p>Оформление отпуска сотрудника через документ отпуск.</p>",
        );
        let mut project_a = Connection::open_in_memory().unwrap();
        let mut project_b = Connection::open_in_memory().unwrap();
        assert_eq!(sync(&mut project_a, "primary", &a).unwrap().added, 1);
        assert!(
            sync(&mut project_a, "primary", &b).is_err(),
            "нельзя незаметно перепривязать source-id"
        );
        assert_eq!(sync(&mut project_a, "extension", &a_ext).unwrap().added, 1);
        assert_eq!(sync(&mut project_b, "primary", &b).unwrap().added, 1);
        let no_op = sync(&mut project_a, "primary", &a).unwrap();
        assert_eq!((no_op.unchanged, no_op.hashed, no_op.parsed), (1, 1, 0));
        std::fs::write(&a_page, "<h1>Заказ клиента</h1><p>Теперь оформление продажи через изменённый документ заказ.</p>").unwrap();
        let changed = sync(&mut project_a, "primary", &a).unwrap();
        assert_eq!((changed.updated, changed.parsed), (1, 1));
        let a_text: String = project_a
            .query_row(
                "SELECT text FROM help_pages WHERE source_id='primary'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(a_text.contains("изменённый"));
        assert_eq!(remove(&mut project_a, "extension").unwrap(), 1);
        assert_eq!(
            project_a
                .query_row("SELECT count(*) FROM help_pages", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            project_b
                .query_row("SELECT count(*) FROM help_pages", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        std::fs::remove_file(a_page).unwrap();
        let deleted = sync(&mut project_a, "primary", &a).unwrap();
        assert_eq!((deleted.removed, deleted.parsed), (1, 0));
        std::fs::remove_dir_all(&a).unwrap();
        assert!(sync(&mut project_a, "primary", &a).is_err());
        assert_eq!(
            project_a
                .query_row("SELECT count(*) FROM help_sources", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            project_a
                .query_row("SELECT count(*) FROM help_pages", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            project_b
                .query_row("SELECT count(*) FROM help_pages", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
