//! Ссылки между объектами метаданных — таблица `metadata_references`.
//!
//! # Почему надстройка, а не разбор
//!
//! Это единственная таблица описи, которая не читает файлы вовсе. 96,2% её
//! строк (84 801 из 88 186 у прежнего инструмента) выводятся из уже разобранного: типы
//! реквизитов, состав подсистем и планов обмена, права ролей, источники
//! подписок, места хранения и состав функциональных опций, определяемые типы.
//!
//! Собирать их вторым разбором XML значило бы завести второй источник тех же
//! данных — и ловить один дефект в двух местах. Расхождение между таблицей
//! ссылок и таблицей-источником при этом было бы невозможно заметить: обе
//! заполнены, обе выглядят рабочими.
//!
//! # Правила нормализации сняты с прежнего инструмента замером, а не приняты по вкусу
//!
//! Форма записи ссылки у него неоднородна, и воспроизводится она как есть:
//!
//! | Источник | Что делает |
//! |---|---|
//! | `object_attributes.attr_type` | `CatalogRef.X` → `Catalog.X` |
//! | `event_subscriptions.source_types` | `DocumentObject.X` → `Document.X` |
//! | `defined_types` | уже нормализовано при разборе |
//! | `role_rights`, `subsystem_content` | как есть |
//!
//! Приводить всё к одному виду «для порядка» нельзя: расхождение формы
//! неотличимо от расхождения данных.

use crate::Result;
use rusqlite::Connection;

/// Одна ссылка между объектами метаданных.
#[derive(Debug, Clone)]
pub struct MetaRef {
    pub source_object: String,
    pub source_category: String,
    pub ref_object: String,
    pub ref_kind: &'static str,
    /// Адрес места, где ссылка встретилась: `Document.X.Attribute.Y.Type`.
    pub used_in: String,
    pub path: String,
    pub line: Option<i64>,
}

/// Виды метаданных, на которые вообще бывают ссылки.
///
/// Белый список прежнего инструмента, снятый с его индекса: во всей таблице `ref_object`
/// встречается ровно 25 префиксов. Восемь видов не встречаются НИ РАЗУ —
/// `Configuration`, `WebService`, `SessionParameter`, `HTTPService`,
/// `FilterCriterion`, `CommonAttribute`, `IntegrationService`, `Sequence`, —
/// хотя права на них у ролей есть (999 пар из 15 374). Значит это не пробел
/// разбора, а сознательный отбор: ссылка ведёт на объект, который можно
/// открыть и посмотреть.
pub const ВИДЫ_ССЫЛОК: &[&str] = &[
    "Document",
    "Catalog",
    "InformationRegister",
    "Constant",
    "Enum",
    "CommonModule",
    "Report",
    "CommonCommand",
    "DataProcessor",
    "AccumulationRegister",
    "CommonForm",
    "Role",
    "Subsystem",
    "DefinedType",
    "ChartOfAccounts",
    "FunctionalOption",
    "EventSubscription",
    "DocumentJournal",
    "ChartOfCharacteristicTypes",
    "ExchangePlan",
    "ScheduledJob",
    "BusinessProcess",
    "ChartOfCalculationTypes",
    "Task",
    "AccountingRegister",
];

/// Виды, у которых бывает ссылочный тип — те, что могут быть ИСТОЧНИКОМ
/// события и хранятся в базе.
///
/// Отличается от [`ВИДЫ_ССЫЛОК`]: обработка, отчёт, журнал документов и
/// последовательность в ссылки от подписок не идут (у прежнего инструмента 74 таких
/// источника не дали ни одной ссылки), потому что ссылочного типа
/// у них не существует — на них нельзя сослаться из данных.
const ССЫЛОЧНЫЕ_ВИДЫ: &[&str] = &[
    "Catalog",
    "Document",
    "Enum",
    "ChartOfCharacteristicTypes",
    "ChartOfAccounts",
    "ChartOfCalculationTypes",
    "ExchangePlan",
    "BusinessProcess",
    "Task",
    "InformationRegister",
    "AccumulationRegister",
    "AccountingRegister",
    "CalculationRegister",
    "Constant",
];

/// Категория выгрузки → вид ссылки: `Documents` → `Document`.
pub fn вид_по_категории(cat: &str) -> Option<&'static str> {
    let v = match cat {
        "Catalogs" => "Catalog",
        "Documents" => "Document",
        "Enums" => "Enum",
        "InformationRegisters" => "InformationRegister",
        "AccumulationRegisters" => "AccumulationRegister",
        "AccountingRegisters" => "AccountingRegister",
        "CalculationRegisters" => "CalculationRegister",
        "ChartsOfAccounts" => "ChartOfAccounts",
        "ChartsOfCharacteristicTypes" => "ChartOfCharacteristicTypes",
        "ChartsOfCalculationTypes" => "ChartOfCalculationTypes",
        "ExchangePlans" => "ExchangePlan",
        "BusinessProcesses" => "BusinessProcess",
        "Tasks" => "Task",
        "Constants" => "Constant",
        "DocumentJournals" => "DocumentJournal",
        "DataProcessors" => "DataProcessor",
        "Reports" => "Report",
        "CommonModules" => "CommonModule",
        "CommonForms" => "CommonForm",
        "CommonCommands" => "CommonCommand",
        "Roles" => "Role",
        "Subsystems" => "Subsystem",
        "DefinedTypes" => "DefinedType",
        "FunctionalOptions" => "FunctionalOption",
        "EventSubscriptions" => "EventSubscription",
        "ScheduledJobs" => "ScheduledJob",
        _ => return None,
    };
    Some(v)
}

/// Снять суффикс вида ссылки, если остаток — ссылочный вид метаданных.
///
/// `DocumentObject.X` → `Document.X`, `ConstantValueManager.X` → `Constant.X`.
/// `DataProcessorManager.X` остаётся как есть и дальше отсеивается: обработка
/// не хранится в базе, ссылки на неё не бывает.
fn нормализовать(t: &str) -> Option<String> {
    let (head, tail) = t.split_once('.')?;
    for s in ["ValueManager", "RecordSet", "Manager", "Object", "Ref"] {
        if let Some(base) = head.strip_suffix(s) {
            if ССЫЛОЧНЫЕ_ВИДЫ.contains(&base) {
                return Some(format!("{base}.{tail}"));
            }
            // Суффикс найден, но вид неподходящий — дальше не перебираем:
            // `DataProcessorManager` это именно `DataProcessor` + `Manager`,
            // а не что-то, что подойдёт под более короткий суффикс.
            return None;
        }
    }
    // Без суффикса вовсе — уже готовая ссылка (`Catalog.X` в составе подсистемы).
    Some(t.to_string())
}

/// Годится ли ссылка: вид цели должен быть в белом списке.
fn годится(r: &str) -> bool {
    r.split_once('.')
        .is_some_and(|(head, _)| ВИДЫ_ССЫЛОК.contains(&head))
}

/// Собрать ссылки из уже заполненных таблиц индекса.
///
/// Читает свой же индекс, а не файлы: к моменту вызова метаданные разобраны.
pub fn собрать(conn: &Connection) -> Result<Vec<MetaRef>> {
    let mut out = Vec::new();

    // --- типы реквизитов ---
    //
    // Только шесть категорий: у прежнего инструмента `attribute_type` встречается ровно
    // у документов, справочников, регистров сведений и накопления, ПВХ и
    // регистров бухгалтерии. У прочих (обработки, отчёты) реквизиты тоже
    // есть, но ссылками они не считаются.
    //
    // # Превышение над прежний инструментом: реквизиты типа `DefinedType.X`
    //
    // Мы даём 2 133 такие ссылки, он — ни одной: в его `attribute_type`
    // девять видов цели, и определяемого типа среди них нет. Ссылки НА
    // определяемый тип у него есть только из состава подсистем (586)
    // и параметров команд (20).
    //
    // Практическое следствие проверено на его индексе: на вопрос «какие
    // реквизиты используют `DefinedType.Организация`» он отвечает пустотой,
    // хотя таких реквизитов две тысячи. Это пробел, а не отбор, — поэтому
    // ссылка остаётся, и превышение объявлено, а не выдано за паритет.
    {
        let mut st = conn.prepare(
            "SELECT object_name, category, attr_name, attr_kind, ts_name,
                    attr_type, source_file
             FROM object_attributes
             WHERE category IN ('Documents','Catalogs','InformationRegisters',
                                'AccumulationRegisters','ChartsOfCharacteristicTypes',
                                'AccountingRegisters')
               AND attr_type IS NOT NULL",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        for row in rows {
            let (obj, cat, attr, kind, ts, types, file) = row?;
            let Some(вид) = вид_по_категории(&cat) else {
                continue;
            };
            for t in разобрать_json_массив(&types) {
                let Some(норм) = нормализовать(&t) else {
                    continue;
                };
                if !годится(&норм) {
                    continue;
                }
                // Адрес места: у реквизита табличной части он длиннее.
                let место = match (&ts, kind.as_str()) {
                    (Some(ts), "ts_attribute") => {
                        format!("{вид}.{obj}.TabularSection.{ts}.Attribute.{attr}.Type")
                    }
                    _ => format!("{вид}.{obj}.{}.{attr}.Type", вид_поля(&kind)),
                };
                out.push(MetaRef {
                    source_object: obj.clone(),
                    source_category: cat.clone(),
                    ref_object: норм,
                    ref_kind: "attribute_type",
                    used_in: место,
                    path: file.clone(),
                    line: None,
                });
            }
        }
    }

    // --- состав подсистем ---
    добавить(
        conn,
        &mut out,
        "SELECT subsystem_name, object_ref, file FROM subsystem_content",
        "subsystem_content",
        "Subsystems",
        |name| format!("Subsystem.{name}.Content"),
    )?;

    // --- права ролей ---
    //
    // Пара (роль, объект), а не каждое право: ссылка отвечает на вопрос
    // «кто трогает объект», а не «каким именно правом». У прежнего инструмента здесь
    // 14 375 строк против 48 458 прав — ровно уникальные пары за вычетом
    // видов вне белого списка.
    добавить(
        conn,
        &mut out,
        "SELECT DISTINCT role_name, object_name, file FROM role_rights",
        "role_rights",
        "Roles",
        |name| format!("Role.{name}.Rights"),
    )?;

    // --- состав планов обмена ---
    добавить(
        conn,
        &mut out,
        "SELECT plan_name, object_ref, path FROM exchange_plan_content",
        "exchange_plan_content",
        "ExchangePlans",
        |name| format!("ExchangePlan.{name}.Content"),
    )?;

    // --- источники подписок ---
    {
        let mut st = conn.prepare("SELECT name, source_types, file FROM event_subscriptions")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (name, types, file) = row?;
            for t in разобрать_json_массив(&types) {
                let Some(норм) = нормализовать(&t) else {
                    continue;
                };
                if !годится(&норм) {
                    continue;
                }
                out.push(MetaRef {
                    source_object: name.clone(),
                    source_category: "EventSubscriptions".into(),
                    ref_object: норм,
                    ref_kind: "event_subscription_source",
                    used_in: format!("EventSubscription.{name}.Source"),
                    path: file.clone().unwrap_or_default(),
                    line: None,
                });
            }
        }
    }

    // --- определяемые типы ---
    {
        let mut st = conn.prepare("SELECT name, type_refs_json, path FROM defined_types")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (name, types, path) = row?;
            for t in разобрать_json_массив(&types) {
                if !годится(&t) {
                    continue;
                }
                out.push(MetaRef {
                    source_object: name.clone(),
                    source_category: "DefinedTypes".into(),
                    ref_object: t,
                    ref_kind: "defined_type_content",
                    used_in: format!("DefinedType.{name}.Type"),
                    path: path.clone(),
                    line: None,
                });
            }
        }
    }

    // --- типы планов видов характеристик ---
    //
    // Примитивы (`String`, `Number`, …) отсеиваются белым списком сами:
    // состав ПВХ мы храним полным, включая их, а ссылкой примитив не является.
    {
        let mut st =
            conn.prepare("SELECT pvh_name, type_refs_json, path FROM characteristic_types")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (name, types, path) = row?;
            for t in разобрать_json_массив(&types) {
                if !годится(&t) {
                    continue;
                }
                out.push(MetaRef {
                    source_object: name.clone(),
                    source_category: "ChartsOfCharacteristicTypes".into(),
                    ref_object: t,
                    ref_kind: "characteristic_type",
                    used_in: format!("ChartOfCharacteristicTypes.{name}.Type"),
                    path: path.clone(),
                    line: None,
                });
            }
        }
    }

    // --- типы предопределённых элементов ПВХ ---
    {
        let mut st = conn.prepare(
            "SELECT object_name, item_name, types_json, source_file
             FROM predefined_items
             WHERE category = 'ChartsOfCharacteristicTypes'
               AND types_json IS NOT NULL AND types_json != '[]'",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (obj, item, types, file) = row?;
            for t in разобрать_json_массив(&types) {
                let Some(норм) = нормализовать(&t) else {
                    continue;
                };
                if !годится(&норм) {
                    continue;
                }
                out.push(MetaRef {
                    source_object: obj.clone(),
                    source_category: "ChartsOfCharacteristicTypes".into(),
                    ref_object: норм,
                    ref_kind: "predefined_characteristic_type",
                    used_in: format!("ChartOfCharacteristicTypes.{obj}.PredefinedItem.{item}.Type"),
                    path: file.clone(),
                    line: None,
                });
            }
        }
    }

    // --- функциональные опции: место хранения и состав ---
    {
        let mut st =
            conn.prepare("SELECT name, location, content, file FROM functional_options")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (name, location, content, file) = row?;
            let file = file.unwrap_or_default();

            // Место хранения: `InformationRegister.X.Resource.Y` → объект `X`.
            if let Some(loc) = location {
                let части: Vec<&str> = loc.split('.').collect();
                if части.len() >= 2 {
                    let цель = format!("{}.{}", части[0], части[1]);
                    if годится(&цель) {
                        out.push(MetaRef {
                            source_object: name.clone(),
                            source_category: "FunctionalOptions".into(),
                            ref_object: цель,
                            ref_kind: "functional_option_content",
                            used_in: format!("FunctionalOption.{name}.Location"),
                            path: file.clone(),
                            line: None,
                        });
                    }
                }
            }

            // Состав: ссылки идут ЦЕЛИКОМ, включая путь до реквизита.
            // Укорачивать до объекта нельзя — опция управляет именно полем.
            for t in разобрать_json_массив(&content) {
                if !годится(&t) {
                    continue;
                }
                out.push(MetaRef {
                    source_object: name.clone(),
                    source_category: "FunctionalOptions".into(),
                    ref_object: t,
                    ref_kind: "functional_option_content",
                    used_in: format!("FunctionalOption.{name}.Content"),
                    path: file.clone(),
                    line: None,
                });
            }
        }
    }

    Ok(out)
}

/// Ссылки, видимые только при разборе XML: ввод на основании, владельцы,
/// формы по умолчанию, тип параметра команды.
///
/// Эти четыре свойства не попадают ни в одну таблицу индекса — ни у нас,
/// ни у прежнего инструмента. Он их тоже хранит только здесь, поэтому заводить под них
/// таблицу-источник значило бы разойтись со схемой ради симметрии.
pub fn из_разбора(
    parsed: &[(crate::meta::MetaObject, Vec<crate::meta::PredefinedItem>)],
) -> Vec<MetaRef> {
    let mut out = Vec::new();
    for (obj, _) in parsed {
        let Some(вид) = вид_по_категории(&obj.category) else {
            continue;
        };
        let сам = format!("{вид}.{}", obj.name);

        for r in &obj.based_on {
            if годится(r) {
                out.push(MetaRef {
                    source_object: obj.name.clone(),
                    source_category: obj.category.clone(),
                    ref_object: r.clone(),
                    ref_kind: "based_on",
                    used_in: format!("{сам}.BasedOn"),
                    path: obj.source_file.clone(),
                    line: None,
                });
            }
        }

        for r in &obj.owners {
            if годится(r) {
                out.push(MetaRef {
                    source_object: obj.name.clone(),
                    source_category: obj.category.clone(),
                    ref_object: r.clone(),
                    ref_kind: "owner",
                    used_in: format!("{сам}.Owners"),
                    path: obj.source_file.clone(),
                    line: None,
                });
            }
        }

        // Только общие команды: у объектов бывают свои команды с тем же
        // свойством, но прежний инструмент их не считает, и это не пробел — параметр
        // объектной команды всегда её же объект, ссылки в этом нет.
        for t in &obj.command_parameter_type {
            if obj.category != "CommonCommands" {
                break;
            }
            // `DefinedType.X` не нормализуется: суффикса у него нет вовсе,
            // а `нормализовать` вернуло бы его как есть — но только если
            // не встретит `Ref` в хвосте `DefinedType`. Проверка порядка
            // тут не нужна, важнее не потерять: у прежнего инструмента таких ссылок 20.
            let норм = нормализовать(t).unwrap_or_else(|| t.clone());
            if годится(&норм) {
                out.push(MetaRef {
                    source_object: obj.name.clone(),
                    source_category: obj.category.clone(),
                    ref_object: норм,
                    ref_kind: "command_parameter_type",
                    used_in: format!("{сам}.CommandParameterType"),
                    path: obj.source_file.clone(),
                    line: None,
                });
            }
        }

        // Формы по умолчанию: цель ссылки — САМ объект, а не форма.
        // Форма стоит в адресе после `=`. Так у прежнего инструмента, и так осмысленно:
        // вопрос «кто ссылается на документ» не должен упираться в его же
        // форму, а вопрос «какая у него форма списка» отвечается адресом.
        for (форма, вид_формы) in [
            (&obj.default_object_form, "DefaultObjectForm"),
            (&obj.default_list_form, "DefaultListForm"),
        ] {
            if let Some(f) = форма {
                if годится(&сам) {
                    out.push(MetaRef {
                        source_object: obj.name.clone(),
                        source_category: obj.category.clone(),
                        ref_object: сам.clone(),
                        ref_kind: if вид_формы == "DefaultObjectForm" {
                            "default_object_form"
                        } else {
                            "default_list_form"
                        },
                        used_in: format!("{сам}.{вид_формы}={f}"),
                        path: obj.source_file.clone(),
                        line: None,
                    });
                }
            }
        }
    }
    out
}

/// Имя секции для адреса реквизита: измерение, ресурс или просто реквизит.
fn вид_поля(kind: &str) -> &'static str {
    match kind {
        "dimension" => "Dimension",
        "resource" => "Resource",
        _ => "Attribute",
    }
}

/// Общий случай: запрос отдаёт (источник, готовая ссылка, файл).
fn добавить(
    conn: &Connection,
    out: &mut Vec<MetaRef>,
    sql: &str,
    kind: &'static str,
    category: &str,
    адрес: impl Fn(&str) -> String,
) -> Result<()> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (source, target, file) = row?;
        if !годится(&target) {
            continue;
        }
        out.push(MetaRef {
            used_in: адрес(&source),
            source_object: source,
            source_category: category.to_string(),
            ref_object: target,
            ref_kind: kind,
            path: file.unwrap_or_default(),
            line: None,
        });
    }
    Ok(())
}

/// Разобрать JSON-массив строк, собранный `json_array`.
///
/// Свой разбор вместо `serde_json` по той же причине, что и своя сборка:
/// значения здесь всегда строки, экранированы кавычка и обратный слэш.
fn разобрать_json_массив(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut внутри = false;
    let mut экран = false;
    for c in s.chars() {
        if экран {
            cur.push(c);
            экран = false;
            continue;
        }
        match c {
            '\\' if внутри => экран = true,
            '"' => {
                if внутри {
                    out.push(std::mem::take(&mut cur));
                }
                внутри = !внутри;
            }
            _ if внутри => cur.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn нормализация_снимает_суффикс_только_у_ссылочных_видов() {
        assert_eq!(
            нормализовать("DocumentObject.А").as_deref(),
            Some("Document.А")
        );
        assert_eq!(нормализовать("CatalogRef.Б").as_deref(), Some("Catalog.Б"));
        assert_eq!(
            нормализовать("InformationRegisterRecordSet.В").as_deref(),
            Some("InformationRegister.В")
        );
        // Составной суффикс снимается целиком, иначе остался бы
        // несуществующий вид `ConstantValue`.
        assert_eq!(
            нормализовать("ConstantValueManager.Г").as_deref(),
            Some("Constant.Г")
        );
        // Обработка и отчёт в базе не хранятся — ссылки на них не бывает.
        assert!(нормализовать("DataProcessorManager.Д").is_none());
        assert!(нормализовать("ReportManager.Е").is_none());
    }

    #[test]
    fn белый_список_отсекает_виды_без_ссылок() {
        assert!(годится("Catalog.Товары"));
        assert!(годится("Role.Администратор"));
        // Права на конфигурацию и параметры сеанса у ролей есть, но
        // ссылкой это не считается: открыть такой объект нельзя.
        assert!(!годится("Configuration.БухгалтерияПредприятия"));
        assert!(!годится("SessionParameter.ТекущийПользователь"));
        assert!(!годится("WebService.Обмен"));
        // Без точки — не ссылка вовсе.
        assert!(!годится("String"));
    }

    #[test]
    fn разбор_json_массива_переживает_экранирование() {
        assert_eq!(разобрать_json_массив("[]"), Vec::<String>::new());
        assert_eq!(
            разобрать_json_массив(r#"["Catalog.А", "Document.Б"]"#),
            vec!["Catalog.А", "Document.Б"]
        );
        assert_eq!(
            разобрать_json_массив(r#"["с \"кавычкой\""]"#),
            vec![r#"с "кавычкой""#]
        );
    }
}
