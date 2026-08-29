//! Перехваты расширений с адресом — таблица `extension_overrides`.
//!
//! Это место, где адрес перехвата — наше преимущество, и терять здесь
//! нельзя ничего:
//! перехвата — модуль, строка, вид — то, ради чего сервер вообще спрашивают
//! про расширения.
//!
//! # Где лежат расширения
//!
//! Не внутри `src`, а СОСЕДЯМИ от него: `<проект>/ext/<Имя расширения>/…`
//! (у части конфигураций каталог зовётся `cfe`). Внутри — обычная выгрузка
//! со своим `Configuration.xml`, поэтому разбор модуля тот же, что у основной
//! конфигурации; отличается только связывание с целью.
//!
//! # Два адреса, и они про разное
//!
//! * `ext_line` — строка САМОЙ АННОТАЦИИ в расширении. Проверено на корпусе ДО
//!   поимённо: прежний инструмент пишет строку `&После(...)`, а не строку следующего за ней
//!   `Процедура`. Разница в одну строку на всех 13 перехватах — ровно тот
//!   случай, где паритет разошёлся бы целиком.
//! * `target_method_line` — строка объявления перехватываемого метода в
//!   ОСНОВНОЙ конфигурации. Пусто, когда цель не разрешилась: модуля нет
//!   или метод в нём не найден. Пустое поле здесь — честный признак
//!   неразрешённого, а не «перехвата без адреса».
//!
//! # Как отличается перехват от директивы компиляции
//!
//! По ФОРМЕ, а не по списку имён (см. `module::override_of`): у перехвата есть
//! строковый аргумент, у `&НаСервере` его нет. Список видов не зашит —
//! `Вместо`, `Перед`, `После`, `ИзменениеИКонтроль` и то, что платформа
//! добавит завтра, разбираются одинаково.
//!
//! # Оговорка о корпусе
//!
//! Сверка сделана на ДО (9 расширений, 13 перехватов): там встречаются только
//! `ИзменениеИКонтроль` (10) и `После` (3). `Вместо` и `Перед` разбираются тем
//! же кодом, но НА ЖИВОМ КОРПУСЕ НЕ ПРОВЕРЕНЫ — сказано прямо, чтобы разбор
//! этих двух не считался подтверждённым.

use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::{Path, PathBuf};

/// Расширение как целое: манифест плюс корень на диске.
#[derive(Debug, Clone)]
pub struct Extension {
    pub name: String,
    /// `Customization` / `Patch` из `<ConfigurationExtensionPurpose>`.
    ///
    /// Тег зовётся именно так, а не `Purpose`: имя проверено в выгрузке.
    pub purpose: Option<String>,
    /// Префикс имён (`Расш1_`) — не нужен для адреса, но объясняет,
    /// почему перехватчик называется не как цель.
    pub name_prefix: Option<String>,
    pub root: PathBuf,
}

/// Один перехват — строка таблицы.
#[derive(Debug, Clone)]
pub struct Override {
    /// Объект основной конфигурации, чей модуль перехвачен:
    /// имя общего модуля либо имя справочника/документа.
    pub object_name: String,
    /// Путь модуля-цели относительно `src`.
    pub source_path: String,
    pub target_method: String,
    /// Строка объявления цели; `None` — цель не разрешилась.
    pub target_method_line: Option<u32>,
    pub annotation: String,
    pub extension_name: String,
    pub extension_purpose: Option<String>,
    pub extension_method: String,
    pub extension_root: String,
    /// Путь модуля расширения относительно корня расширения.
    pub ext_module_path: String,
    pub ext_line: u32,
}

/// Каталоги расширений рядом с `src`.
///
/// Ищем соседей: `<родитель src>/ext` и `<родитель src>/cfe` — обе раскладки
/// встречаются, и какая именно, зависит от конфигурации, а не от нашего выбора.
pub fn extension_dirs(src: &Path) -> Vec<PathBuf> {
    let Some(parent) = src.parent() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in ["ext", "cfe"] {
        let dir = parent.join(name);
        if dir.is_dir() {
            out.push(dir);
        }
    }
    out
}

/// Расширения внутри каталога-контейнера: по одному подкаталогу на расширение.
pub fn collect_extensions(dir: &Path) -> Vec<Extension> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let root = e.path();
        if !root.is_dir() {
            continue;
        }
        let manifest = root.join("Configuration.xml");
        if !manifest.is_file() {
            continue;
        }
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let (purpose, name_prefix) = parse_manifest(&manifest);
        out.push(Extension {
            name,
            purpose,
            name_prefix,
            root,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Назначение и префикс из манифеста расширения.
fn parse_manifest(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(true);

    let mut purpose = None;
    let mut prefix = None;
    let mut current: Option<&'static str> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                current = match local_name(e.name().as_ref()) {
                    b"ConfigurationExtensionPurpose" => Some("purpose"),
                    b"NamePrefix" => Some("prefix"),
                    _ => None,
                };
            }
            Ok(Event::Text(t)) => {
                if let Some(field) = current {
                    if let Ok(v) = t.unescape() {
                        let v = v.trim().to_string();
                        if !v.is_empty() {
                            match field {
                                "purpose" => purpose = Some(v),
                                _ => prefix = Some(v),
                            }
                        }
                    }
                }
            }
            Ok(Event::End(_)) => current = None,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    (purpose, prefix)
}

/// Локальное имя тега без префикса пространства имён.
fn local_name(raw: &[u8]) -> &[u8] {
    match raw.iter().rposition(|b| *b == b':') {
        Some(i) => &raw[i + 1..],
        None => raw,
    }
}

/// Объект-владелец модуля по пути внутри выгрузки.
///
/// `CommonModules/ОбзорДокумента/Ext/Module.bsl` → `ОбзорДокумента`;
/// `Catalogs/ДокументыПредприятия/Forms/ФормаЭлемента/Ext/Form/Module.bsl`
/// → `ДокументыПредприятия`. Второй сегмент — имя объекта в обеих раскладках.
pub fn object_of_path(rel: &str) -> String {
    rel.replace('\\', "/")
        .split('/')
        .nth(1)
        .unwrap_or_default()
        .to_string()
}
