//! Схемы компоновки данных (СКД) — таблицы `dcs_*`.
//!
//! СКД — макет с типом `DataCompositionSchema`: наборы данных (запрос,
//! объект, объединение), поля с ролями, вычисляемые поля, ресурсы с
//! выражениями итогов, параметры и варианты настроек. Без неё в индексе
//! отчёт — это модуль объекта и формы, а ответ на «откуда берётся
//! показатель» лежит в XML макета, который агенту пришлось бы читать
//! целиком (у «Ведомости по товарам на складах» УТ — 187 КБ).
//!
//! # Где лежит
//!
//! ```text
//! <Категория>/<Объект>/Templates/<Макет>/Ext/Template.xml   — макет объекта
//! CommonTemplates/<Макет>/Ext/Template.xml                  — общий макет
//! ```
//!
//! Тип макета узнаётся по корню самого файла (`<DataCompositionSchema`), а
//! не по описанию макета рядом: читается начало файла, табличные макеты в
//! мегабайты не разбираются вовсе.
//!
//! Основная схема отчёта — свойство `MainDataCompositionSchema` в описании
//! отчёта (`Report.<Имя>.Template.<Макет>`).
//!
//! # Источники
//!
//! Из текста запроса каждого набора — упоминания объектов метаданных тем же
//! разборщиком, что и литералы кода (`gyrfalcon_parser::usages`). Три вида:
//! `source` — таблица запроса (`РегистрНакопления.Х.ОстаткиИОбороты`),
//! `type` — ссылочный тип (`ВЫРАЗИТЬ(… КАК СправочникСсылка.Х)`), `value` —
//! значение внутри `ЗНАЧЕНИЕ(…)`. Значения отделены, потому что
//! `ЗНАЧЕНИЕ(Перечисление.Х.Y)` — не чтение данных, и вопрос «какие отчёты
//! читают Х» без этого разделения отвечал бы неправду.
//!
//! Происхождение отдельного поля (из какого столбца источника) здесь НЕ
//! восстанавливается: для этого нужен разбор запроса с псевдонимами `КАК`.
//! Источники — на уровне набора данных.

use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Схема компоновки целиком.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    pub owner_category: String,
    pub owner_name: String,
    pub template: String,
    pub is_main: bool,
    pub file: String,
    pub datasets: Vec<DataSet>,
    pub fields: Vec<Field>,
    pub parameters: Vec<Parameter>,
    pub variants: Vec<Variant>,
}

/// Набор данных. У вложенного в объединение заполнен `parent`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DataSet {
    pub name: String,
    /// `Query` | `Object` | `Union`.
    pub kind: String,
    pub parent: String,
    pub query: String,
    /// Имя объекта у набора-объекта (`DataSetObject`).
    pub object_name: String,
    pub sources: Vec<Source>,
}

/// Упоминание объекта метаданных в тексте запроса набора.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Source {
    /// Английская форма: `AccumulationRegister.ТоварыНаСкладах`.
    pub object_ref: String,
    /// Что стоит после имени: виртуальная таблица, табличная часть.
    pub member: String,
    /// `source` | `type` | `value`.
    pub kind: &'static str,
}

/// Поле схемы: поле набора, вычисляемое поле или ресурс.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Field {
    /// `field` | `calculated` | `total`.
    pub kind: &'static str,
    /// Набор данных — только у `field`.
    pub dataset: String,
    pub data_path: String,
    /// Имя поля в наборе (`<field>`) — только у `field`.
    pub field: String,
    pub title: String,
    /// Выражение — у `calculated` и `total`.
    pub expression: String,
    /// Роль поля одной строкой: `dimension`, `periodNumber=2,periodType=Main`.
    pub role: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parameter {
    pub name: String,
    pub title: String,
    pub expression: String,
    /// Свойство `use` (`Always`, `Auto`); пусто — умолчание.
    pub use_: String,
    pub value_type: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Variant {
    pub name: String,
    pub presentation: String,
}

// ---------------------------------------------------------------- дерево

/// Узел XML: локальное имя, `xsi:type` без префикса, текст, потомки.
#[derive(Debug, Default)]
struct Node {
    name: String,
    typ: String,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    fn text_of(&self, name: &str) -> String {
        self.child(name).map(|c| c.text.clone()).unwrap_or_default()
    }
    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    /// Текст многоязычной строки: русский вариант, иначе первый.
    fn local_string(&self) -> String {
        let items: Vec<&Node> = self.all("item").collect();
        items
            .iter()
            .find(|i| i.text_of("lang") == "ru")
            .or(items.first())
            .map(|i| i.text_of("content"))
            .unwrap_or_else(|| self.text.clone())
    }
}

fn local(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.rsplit(':').next().unwrap_or("").to_string()
}

fn tree(text: &str) -> Option<Node> {
    let mut reader = Reader::from_reader(text.as_bytes());
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = false;
    let mut stack: Vec<Node> = vec![Node::default()];
    let mut buf = Vec::new();
    let open = |e: &quick_xml::events::BytesStart| Node {
        name: local(e.name().as_ref()),
        typ: e
            .attributes()
            .flatten()
            .find(|a| local(a.key.as_ref()) == "type")
            .map(|a| local(a.value.as_ref()))
            .unwrap_or_default(),
        ..Node::default()
    };
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(XmlEvent::Start(e)) => stack.push(open(&e)),
            Ok(XmlEvent::Empty(e)) => {
                let n = open(&e);
                stack.last_mut()?.children.push(n);
            }
            Ok(XmlEvent::Text(t)) => {
                let s = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                stack.last_mut()?.text.push_str(&s);
            }
            Ok(XmlEvent::CData(t)) => {
                stack
                    .last_mut()?
                    .text
                    .push_str(&String::from_utf8_lossy(&t.into_inner()));
            }
            Ok(XmlEvent::End(_)) => {
                let n = stack.pop()?;
                stack.last_mut()?.children.push(n);
            }
            Ok(XmlEvent::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
    let mut корень = stack.pop()?;
    корень.children.pop()
}

// ---------------------------------------------------------------- разбор

fn value_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)(ЗНАЧЕНИЕ|VALUE)\s*\(([^)]*)\)").unwrap())
}

/// Упоминания объектов в тексте запроса, без повторов.
pub fn sources(query: &str) -> Vec<Source> {
    use gyrfalcon_parser::usages::{упоминания_в_литерале, UsageKind};
    // Комментарии запроса — не запрос: закомментированная таблица не читается.
    let без_комм: String = query
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out: Vec<Source> = Vec::new();
    let mut found = Vec::new();
    for c in value_re().captures_iter(&без_комм) {
        упоминания_в_литерале(&c[2], 0, &mut found);
        for u in found.drain(..) {
            out.push(Source {
                object_ref: u.object_ref,
                member: u.member_path.unwrap_or_default(),
                kind: "value",
            });
        }
    }
    let остаток = value_re().replace_all(&без_комм, " ");
    for line in остаток.lines() {
        упоминания_в_литерале(line, 0, &mut found);
    }
    for u in found {
        out.push(Source {
            object_ref: u.object_ref,
            member: u.member_path.unwrap_or_default(),
            kind: match u.kind {
                UsageKind::RefType => "type",
                _ => "source",
            },
        });
    }
    out.sort();
    out.dedup();
    out
}

fn role_of(n: &Node) -> String {
    n.child("role")
        .map(|r| {
            r.children
                .iter()
                .map(|c| {
                    if c.text == "true" {
                        c.name.clone()
                    } else {
                        format!("{}={}", c.name, c.text)
                    }
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

fn title_of(n: &Node) -> String {
    n.child("title").map(Node::local_string).unwrap_or_default()
}

fn dataset(n: &Node, parent: &str, s: &mut Schema) {
    let name = n.text_of("name");
    let query = n.text_of("query");
    s.datasets.push(DataSet {
        name: name.clone(),
        kind: n.typ.strip_prefix("DataSet").unwrap_or(&n.typ).to_string(),
        parent: parent.to_string(),
        sources: sources(&query),
        query,
        object_name: n.text_of("objectName"),
    });
    for f in n.all("field") {
        // Папка полей — группировка в интерфейсе, а не поле.
        if f.typ == "DataSetFieldFolder" {
            continue;
        }
        s.fields.push(Field {
            kind: "field",
            dataset: name.clone(),
            data_path: f.text_of("dataPath"),
            field: f.text_of("field"),
            title: title_of(f),
            role: role_of(f),
            ..Field::default()
        });
    }
    for item in n.all("item") {
        dataset(item, &name, s);
    }
}

/// Разобрать текст СКД. `None` — не СКД или испорченный XML.
pub fn parse_text(text: &str) -> Option<Schema> {
    let root = tree(text)?;
    if root.name != "DataCompositionSchema" {
        return None;
    }
    let mut s = Schema::default();
    for c in &root.children {
        match c.name.as_str() {
            "dataSet" => dataset(c, "", &mut s),
            "calculatedField" => s.fields.push(Field {
                kind: "calculated",
                data_path: c.text_of("dataPath"),
                title: title_of(c),
                expression: c.text_of("expression"),
                ..Field::default()
            }),
            "totalField" => s.fields.push(Field {
                kind: "total",
                data_path: c.text_of("dataPath"),
                expression: c.text_of("expression"),
                ..Field::default()
            }),
            "parameter" => s.parameters.push(Parameter {
                name: c.text_of("name"),
                title: title_of(c),
                expression: c.text_of("expression"),
                use_: c.text_of("use"),
                value_type: c
                    .child("valueType")
                    .map(|v| {
                        v.all("Type")
                            .map(|t| t.text.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default(),
            }),
            "settingsVariant" => s.variants.push(Variant {
                name: c.text_of("name"),
                presentation: c
                    .child("presentation")
                    .map(Node::local_string)
                    .unwrap_or_default(),
            }),
            _ => {}
        }
    }
    Some(s)
}

// ---------------------------------------------------------------- файлы

/// Начинается ли файл со схемы компоновки. Читается только начало:
/// табличный макет бывает в мегабайты.
fn это_скд(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 512];
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let n = f.read(&mut head).unwrap_or(0);
    String::from_utf8_lossy(&head[..n]).contains("<DataCompositionSchema")
}

/// Макет-СКД в выгрузке: (категория, владелец, макет, путь к `Template.xml`).
pub type TemplateFile = (String, String, String, PathBuf);

/// Макеты-СКД одной выгрузки.
pub fn files(root: &Path) -> Vec<TemplateFile> {
    let mut out = Vec::new();
    let dirs = |p: &Path| -> Vec<(String, PathBuf)> {
        std::fs::read_dir(p)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path())))
                    .collect()
            })
            .unwrap_or_default()
    };
    for (cat, cat_path) in dirs(root) {
        if cat == "CommonTemplates" {
            for (t, p) in dirs(&cat_path) {
                let f = p.join("Ext").join("Template.xml");
                if f.is_file() && это_скд(&f) {
                    out.push((cat.clone(), t.clone(), t, f));
                }
            }
            continue;
        }
        for (obj, obj_path) in dirs(&cat_path) {
            for (t, p) in dirs(&obj_path.join("Templates")) {
                let f = p.join("Ext").join("Template.xml");
                if f.is_file() && это_скд(&f) {
                    out.push((cat.clone(), obj.clone(), t, f));
                }
            }
        }
    }
    out.sort_by(|a, b| a.3.cmp(&b.3));
    out
}

/// Имя основной схемы отчёта из его описания, если она задана.
fn основная_схема(root: &Path, category: &str, owner: &str) -> Option<String> {
    let text = std::fs::read_to_string(root.join(category).join(format!("{owner}.xml"))).ok()?;
    let start = text.find("<MainDataCompositionSchema>")? + "<MainDataCompositionSchema>".len();
    let end = text[start..].find('<')? + start;
    text[start..end]
        .rsplit(".Template.")
        .next()
        .map(str::to_string)
}

/// Разобрать файл СКД с адресом владельца.
pub fn parse_file(
    root: &Path,
    category: &str,
    owner: &str,
    template: &str,
    path: &Path,
    rel: &str,
) -> Option<Schema> {
    let raw = std::fs::read(path).ok()?;
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
    let mut s = parse_text(&String::from_utf8_lossy(raw))?;
    s.owner_category = category.to_string();
    s.owner_name = owner.to_string();
    s.template = template.to_string();
    s.file = rel.to_string();
    s.is_main = основная_схема(root, category, owner).as_deref() == Some(template);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const СКД: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DataCompositionSchema xmlns="http://v8.1c.ru/8.1/data-composition-system/schema" xmlns:dcscom="http://v8.1c.ru/8.1/data-composition-system/common" xmlns:dcscor="http://v8.1c.ru/8.1/data-composition-system/core" xmlns:dcsset="http://v8.1c.ru/8.1/data-composition-system/settings" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
	<dataSource><name>ИсточникДанных1</name><dataSourceType>Local</dataSourceType></dataSource>
	<dataSet xsi:type="DataSetQuery">
		<name>НаборДанных</name>
		<field xsi:type="DataSetFieldField">
			<dataPath>Номенклатура</dataPath><field>Номенклатура</field>
			<title xsi:type="v8:LocalStringType"><v8:item><v8:lang>ru</v8:lang><v8:content>Номенклатура</v8:content></v8:item></title>
			<role><dcscom:dimension>true</dcscom:dimension></role>
		</field>
		<field xsi:type="DataSetFieldFolder"><dataPath>Папка</dataPath></field>
		<field xsi:type="DataSetFieldField">
			<dataPath>ПериодСекунда</dataPath><field>ПериодСекунда</field>
			<role><dcscom:periodNumber>2</dcscom:periodNumber><dcscom:periodType>Main</dcscom:periodType></role>
		</field>
		<dataSource>ИсточникДанных1</dataSource>
		<query>ВЫБРАТЬ
	Т.Номенклатура КАК Номенклатура
ИЗ
	РегистрНакопления.ТоварыНаСкладах.ОстаткиИОбороты(&amp;НачалоПериода, , , , ) КАК Т
	// РегистрСведений.Закомментирован КАК З
ГДЕ
	Т.Операция = ЗНАЧЕНИЕ(Перечисление.ХозяйственныеОперации.Реализация)
	И Т.Регистратор ССЫЛКА Документ.РеализацияТоваровУслуг</query>
	</dataSet>
	<dataSet xsi:type="DataSetUnion">
		<name>Объединение</name>
		<item xsi:type="DataSetQuery">
			<name>Часть1</name>
			<query>ВЫБРАТЬ 1 ИЗ Справочник.Склады.Ячейки КАК С</query>
		</item>
	</dataSet>
	<calculatedField><dataPath>Единица</dataPath><expression>ВЫБОР КОГДА &amp;Ед = 0 ТОГДА 1 КОНЕЦ</expression></calculatedField>
	<totalField><dataPath>Приход</dataPath><expression>Сумма(Приход)</expression></totalField>
	<parameter>
		<name>НачалоПериода</name>
		<title xsi:type="v8:LocalStringType"><v8:item><v8:lang>ru</v8:lang><v8:content>Начало периода</v8:content></v8:item></title>
		<valueType><v8:Type>xs:dateTime</v8:Type></valueType>
		<expression>&amp;Период.ДатаНачала</expression>
		<use>Always</use>
	</parameter>
	<settingsVariant>
		<dcsset:name>Основной</dcsset:name>
		<dcsset:presentation xsi:type="v8:LocalStringType"><v8:item><v8:lang>ru</v8:lang><v8:content>Основной вариант</v8:content></v8:item></dcsset:presentation>
	</settingsVariant>
</DataCompositionSchema>"#;

    #[test]
    fn разбирает_наборы_поля_параметры_варианты() {
        let s = parse_text(СКД).unwrap();
        let имена: Vec<(&str, &str, &str)> = s
            .datasets
            .iter()
            .map(|d| (d.name.as_str(), d.kind.as_str(), d.parent.as_str()))
            .collect();
        assert_eq!(
            имена,
            [
                ("НаборДанных", "Query", ""),
                ("Объединение", "Union", ""),
                ("Часть1", "Query", "Объединение")
            ]
        );
        let поля: Vec<(&str, &str, &str)> = s
            .fields
            .iter()
            .map(|f| (f.kind, f.data_path.as_str(), f.role.as_str()))
            .collect();
        assert_eq!(
            поля,
            [
                ("field", "Номенклатура", "dimension"),
                ("field", "ПериодСекунда", "periodNumber=2,periodType=Main"),
                ("calculated", "Единица", ""),
                ("total", "Приход", "")
            ],
            "папка полей — не поле"
        );
        assert_eq!(s.fields[0].title, "Номенклатура");
        assert_eq!(s.fields[2].expression, "ВЫБОР КОГДА &Ед = 0 ТОГДА 1 КОНЕЦ");
        assert_eq!(
            s.parameters,
            [Parameter {
                name: "НачалоПериода".into(),
                title: "Начало периода".into(),
                expression: "&Период.ДатаНачала".into(),
                use_: "Always".into(),
                value_type: "xs:dateTime".into(),
            }]
        );
        assert_eq!(
            s.variants,
            [Variant {
                name: "Основной".into(),
                presentation: "Основной вариант".into()
            }]
        );
    }

    #[test]
    fn источники_разделены_на_чтение_тип_и_значение() {
        let s = parse_text(СКД).unwrap();
        let src: Vec<(&str, &str, &str)> = s.datasets[0]
            .sources
            .iter()
            .map(|x| (x.object_ref.as_str(), x.member.as_str(), x.kind))
            .collect();
        assert!(
            src.contains(&(
                "AccumulationRegister.ТоварыНаСкладах",
                "ОстаткиИОбороты",
                "source"
            )),
            "{src:?}"
        );
        assert!(
            src.contains(&("Enum.ХозяйственныеОперации", "Реализация", "value")),
            "{src:?}"
        );
        assert!(
            !src.iter()
                .any(|x| x.0 == "Enum.ХозяйственныеОперации" && x.2 == "source"),
            "значение не чтение: {src:?}"
        );
        assert!(
            !src.iter().any(|x| x.0.contains("Закомментирован")),
            "закомментированное не читается: {src:?}"
        );
        assert_eq!(
            s.datasets[2].sources,
            [Source {
                object_ref: "Catalog.Склады".into(),
                member: "Ячейки".into(),
                kind: "source"
            }]
        );
    }

    #[test]
    fn не_скд_не_разбирается() {
        assert!(parse_text("<document><x/></document>").is_none());
    }

    #[test]
    fn файлы_и_основная_схема() {
        let dir = std::env::temp_dir().join("gyrfalcon-dcs-files");
        let _ = std::fs::remove_dir_all(&dir);
        let t = dir.join("Reports/Ведомость/Templates/Схема/Ext");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("Template.xml"), СКД).unwrap();
        let m = dir.join("Reports/Ведомость/Templates/Макет/Ext");
        std::fs::create_dir_all(&m).unwrap();
        std::fs::write(m.join("Template.xml"), "<document/>").unwrap();
        let c = dir.join("CommonTemplates/Общая/Ext");
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(c.join("Template.xml"), СКД).unwrap();
        std::fs::write(
            dir.join("Reports/Ведомость.xml"),
            "<Report><MainDataCompositionSchema>Report.Ведомость.Template.Схема</MainDataCompositionSchema></Report>",
        )
        .unwrap();
        let f = files(&dir);
        let имена: Vec<(&str, &str, &str)> = f
            .iter()
            .map(|(к, в, м, _)| (к.as_str(), в.as_str(), м.as_str()))
            .collect();
        assert_eq!(имена.len(), 2, "табличный макет не СКД: {имена:?}");
        assert!(имена.contains(&("Reports", "Ведомость", "Схема")));
        assert!(имена.contains(&("CommonTemplates", "Общая", "Общая")));
        let (к, в, м, п) = f.iter().find(|x| x.0 == "Reports").unwrap();
        let s = parse_file(&dir, к, в, м, п, "Reports/x").unwrap();
        assert!(s.is_main);
    }
}
