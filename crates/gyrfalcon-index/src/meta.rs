//! Разбор XML метаданных конфигурации — веха 3, ядро.
//!
//! # Что здесь разбирается
//!
//! Реквизиты объектов с типами, синонимы, значения перечислений,
//! предопределённые элементы, определяемые типы и планы видов характеристик,
//! состав подсистем. Это то, что агент спрашивает чаще всего: «какие поля
//! у документа», «какого типа реквизит», «что входит в подсистему».
//!
//! # Форма выгрузки — снята с живой БП 3.0.190.11, а не по памяти
//!
//! Объект лежит в `<MetaDataObject>` → `<Catalog|Document|…>` с двумя ветками:
//! `<Properties>` (свойства самого объекта) и `<ChildObjects>` (его состав).
//! Внутри `ChildObjects` подчинённые лежат **плоским списком** — `<Attribute>`,
//! `<Dimension>`, `<Resource>`, `<TabularSection>`, `<EnumValue>` подряд,
//! без обёртки вида `<Attributes>`. Ровно на этом обычно теряют данные:
//! разбор, ищущий секцию-контейнер, находит пустоту и молчит.
//!
//! Табличная часть — тот же `ChildObjects`, вложенный на уровень глубже;
//! её реквизиты у прежнего инструмента помечены `attr_kind = ts_attribute` и несут `ts_name`.
//!
//! # Плоский XML: главная ловушка вехи
//!
//! Объект выгружается **файлом без каталога-спутника**, если у него нет форм
//! и модулей: `Catalogs/Имя.xml` есть, `Catalogs/Имя/` — нет. На БП таких
//! 40 из 706 справочников. Обход по каталогам их теряет, а `search` находит,
//! и молчание выглядит как «нет такого объекта». Поэтому обход здесь идёт
//! **по файлам `*.xml` первого уровня категории**, а не по подкаталогам —
//! наличие каталога-спутника вообще не проверяется.
//!
//! # Квалификаторы типов: здесь мы точнее прежнего инструмента
//!
//! Прежний инструмент кладёт в `attr_type` JSON-массив голых имён: `["String"]`, `["Number"]`.
//! Замер 28.08.2026 по его индексу БП: 1706 уникальных обозначений типа,
//! из них **ни одного** с длиной или точностью — 10 891 строковый реквизит
//! и 7 200 числовых лежат без квалификаторов. Мы храним их отдельными
//! столбцами (`length`, `precision`, `scale`, `date_fractions`), а `attr_type`
//! оставляем в его формате — иначе сверка «поимённо» превращается в сверку
//! с переводом.

use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::Path;

/// Реквизит объекта: то, что попадёт в `object_attributes`.
#[derive(Debug, Clone, Default)]
pub struct Attribute {
    pub name: String,
    pub synonym: Option<String>,
    /// Типы как JSON-массив имён — формат прежнего инструмента, для сверки поимённо.
    pub types: Vec<String>,
    /// `attribute` | `ts_attribute` | `dimension` | `resource`.
    pub kind: &'static str,
    /// Имя табличной части для `ts_attribute`.
    pub ts_name: Option<String>,
    // --- квалификаторы: сверх прежнего инструмента ---
    pub length: Option<u32>,
    pub precision: Option<u32>,
    pub scale: Option<u32>,
    pub date_fractions: Option<String>,
}

/// Предопределённый элемент из `Ext/Predefined.xml`.
#[derive(Debug, Clone, Default)]
pub struct PredefinedItem {
    pub name: String,
    pub code: Option<String>,
    pub description: Option<String>,
    pub is_folder: bool,
    /// Типы значения — только у планов видов характеристик.
    /// У предопределённого элемента справочника их не бывает.
    pub types: Vec<String>,
}

/// Разобранный объект метаданных.
#[derive(Debug, Clone, Default)]
pub struct MetaObject {
    /// Имя объекта (`Properties/Name` верхнего уровня).
    pub name: String,
    /// Категория = каталог выгрузки: `Catalogs`, `Documents`, …
    pub category: String,
    pub synonym: Option<String>,
    pub attributes: Vec<Attribute>,
    /// Значения перечисления — только для `Enums`.
    pub enum_values: Vec<String>,
    /// Ссылки на типы: для `DefinedTypes` и `ChartsOfCharacteristicTypes`.
    pub type_refs: Vec<String>,
    /// Состав подсистемы: ссылки вида `Catalog.Номенклатура`.
    pub content: Vec<String>,
    /// Ввод на основании: `<BasedOn>` документа.
    pub based_on: Vec<String>,
    /// Владельцы подчинённого справочника: `<Owners>`.
    pub owners: Vec<String>,
    /// Тип параметра команды: `<CommandParameterType>`.
    pub command_parameter_type: Vec<String>,
    /// Форма объекта по умолчанию: `Document.X.Form.ФормаДокумента`.
    pub default_object_form: Option<String>,
    /// Форма списка по умолчанию.
    pub default_list_form: Option<String>,
    /// Путь файла относительно корня выгрузки — как у прежнего инструмента, через `/`.
    pub source_file: String,
    /// Разбор споткнулся. Не «файла нет» — именно XML оказался неразбираем.
    pub had_error: bool,
}

/// Категории объектов метаданных.
///
/// # Почему список такой длинный
///
/// Соблазн ограничиться теми, у кого бывают реквизиты, — ошибка: **синоним
/// есть у всякого объекта**, включая общие модули, роли и команды. Сверка
/// с прежним инструментом 28.08.2026 показала ровно это: при коротком списке терялось
/// 7 442 синонима из 13 497, причём молча — таблица заполнялась и выглядела
/// рабочей. Категории ниже сняты с его индекса поимённо (`object_synonyms`).
///
/// Каталоги без объектов метаданных (`Ext`, `CommonPictures`) сюда не входят:
/// обходить их — тратить время на заведомо пустое.
pub const CATEGORIES: &[&str] = &[
    "Catalogs",
    "Documents",
    "InformationRegisters",
    "AccumulationRegisters",
    "AccountingRegisters",
    "CalculationRegisters",
    "ChartsOfCharacteristicTypes",
    "ChartsOfAccounts",
    "ChartsOfCalculationTypes",
    "Enums",
    "DefinedTypes",
    "Subsystems",
    "BusinessProcesses",
    "Tasks",
    "ExchangePlans",
    "DocumentJournals",
    "Constants",
    "Reports",
    "DataProcessors",
    // Реквизитов не имеют, синонимы имеют — и их спрашивают не реже.
    "CommonModules",
    "Roles",
    "CommonCommands",
    "CommonForms",
    "CommonTemplates",
    "CommonAttributes",
    "FunctionalOptions",
    "FunctionalOptionsParameters",
    "EventSubscriptions",
    "ScheduledJobs",
    "XDTOPackages",
    "WebServices",
    "HTTPServices",
    "FilterCriteria",
    "SettingsStorages",
    "Sequences",
    "DocumentNumerators",
    "CommandGroups",
    "StyleItems",
    "Languages",
];

/// Категории с вложенной иерархией: объект может лежать глубже первого уровня.
///
/// Пока такая одна — подсистемы. Дочерняя подсистема лежит в
/// `Subsystems/<Родитель>/Subsystems/<Дочерняя>.xml`, и обход первого уровня
/// видит 73 файла вместо 961. Сверка 28.08.2026: так терялось 13 223 строки
/// состава из 18 284 — три четверти таблицы, при внешне заполненном индексе.
const NESTED_CATEGORIES: &[&str] = &["Subsystems"];

/// Собрать пути XML-файлов объектов метаданных.
///
/// # Почему первый уровень, а не рекурсия
///
/// Объект — это `<Категория>/<Имя>.xml`. Всё, что глубже, — его составные
/// части (формы, макеты, команды), у них свои XML и свои имена; включать их
/// сюда значило бы посчитать форму объектом. Плоские объекты при этом
/// **не теряются**: они лежат ровно там же, на первом уровне.
pub fn collect_objects(src: &Path) -> Vec<(String, std::path::PathBuf)> {
    let mut out = Vec::new();
    for cat in CATEGORIES {
        let dir = src.join(cat);
        if NESTED_CATEGORIES.contains(cat) {
            collect_nested(&dir, cat, &mut out);
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xml")) {
                out.push(((*cat).to_string(), p));
            }
        }
    }
    out
}

/// Обойти категорию с вложенной иерархией.
///
/// Рекурсия идёт только по одноимённым подкаталогам (`Subsystems/X/Subsystems/…`)
/// и по каталогу-спутнику объекта — а не по всему дереву: внутри лежат ещё
/// формы и макеты со своими XML, и брать их значило бы посчитать форму объектом.
fn collect_nested(dir: &Path, cat: &str, out: &mut Vec<(String, std::path::PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xml")) {
            out.push((cat.to_string(), p));
        } else if p.is_dir() {
            // Спускаемся в `<Объект>/Subsystems/`, а не во всё подряд.
            let nested = p.join(cat);
            if nested.is_dir() {
                collect_nested(&nested, cat, out);
            }
        }
    }
}

/// Разобрать XML объекта метаданных.
pub fn parse_object(path: &Path, category: &str, rel_path: &str) -> Option<MetaObject> {
    let raw = std::fs::read(path).ok()?;
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
    let text = String::from_utf8_lossy(raw);

    let mut obj = MetaObject {
        category: category.to_string(),
        source_file: rel_path.to_string(),
        ..Default::default()
    };

    let mut reader = Reader::from_str(&text);
    let cfg = reader.config_mut();
    cfg.trim_text(true);
    cfg.check_end_names = false;

    // Путь тегов от корня. По нему, а не по отступам, решается,
    // чьё это `<Name>`: объекта, реквизита или вложенного свойства.
    let mut stack: Vec<String> = Vec::new();
    // Текущий разбираемый подчинённый объект.
    let mut cur: Option<Attribute> = None;
    // Имя табличной части, внутри которой идут реквизиты.
    let mut ts_name: Option<String> = None;
    // Глубина `ChildObjects`: 1 — состав объекта, 2 — состав табличной части.
    let mut child_depth = 0usize;
    // Собираем ли сейчас типы (внутри `<Type>`).
    let mut in_type = 0usize;
    // Внутри `<Synonym>` — синоним берётся из `v8:content` первого языка.
    let mut in_synonym = 0usize;
    let mut synonym_taken = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "ChildObjects" => child_depth += 1,
                    // `TypeSet` открывает ту же секцию типов, что и `Type`:
                    // определяемый тип в параметре команды записан именно им.
                    "Type" | "TypeSet" => in_type += 1,
                    "Synonym" => {
                        in_synonym += 1;
                        synonym_taken = false;
                    }
                    // Подчинённые объекты — плоским списком внутри ChildObjects.
                    //
                    // Одно и то же имя тега бывает И подчинённым объектом,
                    // И свойством: `<AccountingFlag/>` внутри измерения регистра —
                    // это ссылка-свойство, а `<AccountingFlag uuid="…">` в плане
                    // счетов — настоящий объект. Различает их НАЛИЧИЕ uuid,
                    // а не имя. Без этой проверки пустой тег-свойство обрывал
                    // разбор текущего измерения: на регистре «Хозрасчетный»
                    // терялось 8 из 11 подчинённых, и молча (сверка 28.08.2026).
                    "Attribute"
                    | "Dimension"
                    | "Resource"
                    | "AccountingFlag"
                    | "ExtDimensionAccountingFlag"
                    | "AddressingAttribute"
                        if имеет_uuid(&e) =>
                    {
                        let kind = match (name.as_str(), child_depth) {
                            (_, d) if d >= 2 => "ts_attribute",
                            ("Dimension", _) => "dimension",
                            ("Resource", _) => "resource",
                            _ => "attribute",
                        };
                        cur = Some(Attribute {
                            kind,
                            ts_name: if child_depth >= 2 {
                                ts_name.clone()
                            } else {
                                None
                            },
                            ..Default::default()
                        });
                    }
                    "TabularSection" => {
                        // Имя ТЧ ещё впереди; пометим, что мы внутри неё.
                        ts_name = Some(String::new());
                        cur = None;
                    }
                    "EnumValue" => cur = Some(Attribute::default()),
                    _ => {}
                }
                stack.push(name);
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "ChildObjects" => {
                        child_depth = child_depth.saturating_sub(1);
                        if child_depth < 1 {
                            ts_name = None;
                        }
                    }
                    "Type" | "TypeSet" => in_type = in_type.saturating_sub(1),
                    "Synonym" => in_synonym = in_synonym.saturating_sub(1),
                    // Закрываем только начатый объект: у тега-свойства
                    // конца в потоке нет (он самозакрывающийся), а вот парный
                    // конец у одноимённого свойства с телом — бывает.
                    "Attribute"
                    | "Dimension"
                    | "Resource"
                    | "AccountingFlag"
                    | "ExtDimensionAccountingFlag"
                    | "AddressingAttribute" => {
                        if let Some(a) = cur.take() {
                            if a.name.is_empty() {
                                // Имени не набрали — значит закрылось свойство,
                                // а не объект. Возвращаем разбор как был.
                                cur = Some(a);
                            } else {
                                obj.attributes.push(a);
                            }
                        }
                    }
                    "EnumValue" => {
                        if let Some(a) = cur.take() {
                            if !a.name.is_empty() {
                                obj.enum_values.push(a.name);
                            }
                        }
                    }
                    "TabularSection" => ts_name = None,
                    _ => {}
                }
                stack.pop();
            }
            Ok(Event::Text(t)) => {
                let Ok(txt) = t.unescape() else { continue };
                let txt = txt.trim();
                if txt.is_empty() {
                    continue;
                }
                let tag = stack.last().map(String::as_str).unwrap_or("");

                // --- типы и квалификаторы ---
                if in_type > 0 {
                    if let Some(a) = cur.as_mut() {
                        apply_type_part(a, tag, txt);
                    } else {
                        // Типы вне подчинённого объекта: DefinedTypes,
                        // планы видов характеристик, реквизиты-ссылки.
                        if tag == "Type" || tag == "TypeSet" {
                            // Тип параметра команды лежит в такой же секции
                            // `<Type>`, но означает другое — куда команда
                            // применяется, а не из чего состоит объект.
                            // Различает их только окружающий тег.
                            // Тег бывает и `Type`, и `TypeSet`: определяемый
                            // тип в параметре команды записан именно вторым
                            // (`<v8:TypeSet>cfg:DefinedType.X`). Условие
                            // только на `Type` теряло 20 ссылок из 610.
                            if stack.iter().any(|s| s == "CommandParameterType") {
                                obj.command_parameter_type.push(strip_ns(txt).to_string());
                            } else {
                                obj.type_refs.push(strip_suffix(strip_ns(txt)));
                            }
                        }
                    }
                    continue;
                }

                match tag {
                    "Name" => {
                        if let Some(a) = cur.as_mut() {
                            if a.name.is_empty() {
                                a.name = txt.to_string();
                            }
                        } else if let Some(ts) = ts_name.as_mut() {
                            if ts.is_empty() {
                                *ts = txt.to_string();
                            }
                        } else if obj.name.is_empty() {
                            obj.name = txt.to_string();
                        }
                    }
                    // Синоним: первый `v8:content` внутри `<Synonym>`.
                    "content" if in_synonym > 0 && !synonym_taken => {
                        synonym_taken = true;
                        if let Some(a) = cur.as_mut() {
                            a.synonym = Some(txt.to_string());
                        } else if obj.synonym.is_none() && ts_name.is_none() {
                            obj.synonym = Some(txt.to_string());
                        }
                    }
                    // Состав подсистемы.
                    "Item" if stack.iter().any(|s| s == "Content") => {
                        obj.content.push(txt.to_string());
                    }
                    // Ввод на основании и владельцы: тег `<xr:Item>` внутри
                    // соответствующей секции. Значение уже готовой формы
                    // (`Document.X`), нормализация не нужна.
                    "Item" if stack.iter().any(|s| s == "BasedOn") => {
                        obj.based_on.push(txt.to_string());
                    }
                    "Item" if stack.iter().any(|s| s == "Owners") => {
                        obj.owners.push(txt.to_string());
                    }
                    "DefaultObjectForm" => obj.default_object_form = Some(txt.to_string()),
                    "DefaultListForm" => obj.default_list_form = Some(txt.to_string()),
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                obj.had_error = true;
                break;
            }
            _ => {}
        }
        buf.clear();
    }

    if obj.name.is_empty() {
        // Имя — единственное, без чего объект не адресуем. Пустое имя это
        // не «объект без имени», а неразобранный файл: отдаём отказ, а не
        // строку-пустышку, которая осядет в индексе и будет считаться фактом.
        return None;
    }
    Some(obj)
}

/// Разложить кусок `<Type>` по полям реквизита.
fn apply_type_part(a: &mut Attribute, tag: &str, txt: &str) {
    match tag {
        "Type" => a.types.push(strip_ns(txt).to_string()),
        // Тип может задаваться не перечислением, а ССЫЛКОЙ на определяемый
        // тип или на характеристики: `<v8:TypeSet>cfg:DefinedType.Имя</v8:TypeSet>`.
        // На БП таких 2 862 вхождения. Обработка только `<Type>` даёт у таких
        // реквизитов пустой список — и «типа нет» становится неотличимо от
        // «тип не разобрали». Состав набора раскрывается по `defined_types`
        // отдельным запросом; здесь хранится сама ссылка, как она записана.
        "TypeSet" => a.types.push(strip_ns(txt).to_string()),
        // Квалификаторы. Прежний инструмент их теряет — мы храним.
        "Length" => a.length = txt.parse().ok(),
        "Digits" => a.precision = txt.parse().ok(),
        "FractionDigits" => a.scale = txt.parse().ok(),
        "DateFractions" => a.date_fractions = Some(txt.to_string()),
        _ => {}
    }
}

/// Перевести имя типа в форму прежнего инструмента: `cfg:CatalogRef.Товар` → `CatalogRef.Товар`,
/// `xs:string` → `String`.
///
/// Именование сознательно повторяет прежнего инструмента, включая его сокращения примитивов:
/// иначе паритет пришлось бы считать через таблицу соответствий, а расхождение
/// в переводе неотличимо от расхождения в данных.
fn strip_ns(t: &str) -> &str {
    let bare = t.rsplit(':').next().unwrap_or(t);
    match bare {
        "string" => "String",
        "decimal" => "Number",
        "boolean" => "Boolean",
        "dateTime" => "DateTime",
        "date" => "Date",
        other => other,
    }
}

/// Снять суффикс вида ссылки: `DocumentRef.X` → `Document.X`.
///
/// # Почему тут снимаем, а в реквизитах нет
///
/// Это не наш вкус, а форма прежнего инструмента, и она у него РАЗНАЯ по таблицам —
/// замер 28.08.2026 по его индексу БП:
///
/// | Где | Суффикс |
/// |---|---|
/// | `object_attributes.attr_type` | **сохраняет** (`Ref` — 17 968 значений) |
/// | `defined_types`, `characteristic_types` | **снимает** (0 значений с суффиксом) |
/// | `event_subscriptions.source_types` | **сохраняет** (`Object`, `Manager`, `RecordSet`) |
///
/// Однородного правила у него нет, и приводить всё к одному виду «для
/// красоты» нельзя: расхождение формы неотличимо от расхождения данных,
/// а сверка поимённо превратится в сверку с переводом.
///
/// Стоила эта разница дорого: 398 определяемых типов из 481 лежали у нас
/// в чужой форме, а прошлая сверка показывала `OK 481/481` — потому что
/// сверяла ИМЕНА, а не значения. Поймано только на сборке `metadata_references`.
fn strip_suffix(t: &str) -> String {
    let Some((head, tail)) = t.split_once('.') else {
        return t.to_string();
    };
    // Суффикс снимается, только если ОСТАТОК — известный вид метаданных.
    // Отсечение хвоста «вообще» ошибается ровно там, где имя вида само
    // кончается на суффикс: `BusinessProcessRoutePointRef.X` прежний инструмент
    // оставляет как есть, потому что `BusinessProcessRoutePoint` — это
    // не объект метаданных, а точка маршрута внутри него. На корпусе БП
    // таких значений 6, и все шесть — этот случай.
    //
    // Порядок — от длинного суффикса к короткому: `ConstantValueManager.X`
    // даёт `Constant.X`, а снятие одного лишь `Manager` оставило бы
    // несуществующее `ConstantValue`.
    for s in ["ValueManager", "RecordSet", "Manager", "Object", "Ref"] {
        if let Some(base) = head.strip_suffix(s) {
            if ВИДЫ_МЕТАДАННЫХ.contains(&base) {
                return format!("{base}.{tail}");
            }
        }
    }
    t.to_string()
}

/// Виды метаданных, у которых бывают ссылочные типы.
///
/// Список нужен не для красоты: он отличает `DocumentRef.X` (снять `Ref`)
/// от `BusinessProcessRoutePointRef.X` (не трогать). Снят с фактических
/// значений `<v8:Type>` выгрузки БП, а не выведен из общего знания о 1С.
const ВИДЫ_МЕТАДАННЫХ: &[&str] = &[
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
    "DocumentJournal",
    "DataProcessor",
    "Report",
];

/// Есть ли у тега атрибут `uuid` — признак настоящего объекта метаданных.
///
/// Свойства пишутся без него: `<AccountingFlag/>`, `<LinkByType/>`.
fn имеет_uuid(e: &quick_xml::events::BytesStart) -> bool {
    e.attributes().flatten().any(|a| a.key.as_ref() == b"uuid")
}

/// Локальное имя тега без префикса пространства имён.
fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

/// Разобрать `Ext/Predefined.xml` рядом с объектом.
///
/// Отсутствие файла — штатный случай (у большинства объектов предопределённых
/// нет), поэтому возвращается пустой список, а не отказ.
pub fn parse_predefined(path: &Path) -> Vec<PredefinedItem> {
    let Ok(raw) = std::fs::read(path) else {
        return Vec::new();
    };
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
    let text = String::from_utf8_lossy(raw);

    let mut out = Vec::new();
    let mut reader = Reader::from_str(&text);
    let cfg = reader.config_mut();
    cfg.trim_text(true);
    cfg.check_end_names = false;

    let mut cur: Option<PredefinedItem> = None;
    let mut tag = String::new();
    // Глубина внутри `<Item>`: у групп бывают вложенные `<Item>`,
    // и без счётчика конец вложенного закрывал бы внешний.
    let mut depth = 0usize;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                tag = local_name(e.name().as_ref());
                if tag == "Item" {
                    if let Some(it) = cur.take() {
                        if !it.name.is_empty() {
                            out.push(it);
                        }
                    }
                    cur = Some(PredefinedItem::default());
                    depth += 1;
                }
            }
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == "Item" {
                    depth = depth.saturating_sub(1);
                    if let Some(it) = cur.take() {
                        if !it.name.is_empty() {
                            out.push(it);
                        }
                    }
                }
                tag.clear();
            }
            Ok(Event::Text(t)) => {
                let Ok(txt) = t.unescape() else { continue };
                let txt = txt.trim();
                if txt.is_empty() || depth == 0 {
                    continue;
                }
                if let Some(it) = cur.as_mut() {
                    match tag.as_str() {
                        // Тип предопределённого: `d4p1:EnumRef.X`. Префикс
                        // пространства имён тут другой (`d4p1`, не `cfg`),
                        // потому что объявлен на самом теге — снимается
                        // тем же `strip_ns`, он смотрит на двоеточие.
                        "Type" => it.types.push(strip_ns(txt).to_string()),
                        "Name" if it.name.is_empty() => it.name = txt.to_string(),
                        "Code" if it.code.is_none() => it.code = Some(txt.to_string()),
                        "Description" if it.description.is_none() => {
                            it.description = Some(txt.to_string());
                        }
                        "IsFolder" => it.is_folder = txt.eq_ignore_ascii_case("true"),
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    if let Some(it) = cur {
        if !it.name.is_empty() {
            out.push(it);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn во_временный_файл(имя: &str, xml: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("gyrfalcon-meta-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(имя);
        std::fs::write(&p, xml).unwrap();
        p
    }

    const СПРАВОЧНИК: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <Catalog uuid="x">
    <Properties>
      <Name>Товары</Name>
      <Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Товары</v8:content></v8:item></Synonym>
    </Properties>
    <ChildObjects>
      <Attribute uuid="a1">
        <Properties>
          <Name>Артикул</Name>
          <Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Артикул</v8:content></v8:item></Synonym>
          <Type>
            <v8:Type>xs:string</v8:Type>
            <v8:StringQualifiers><v8:Length>25</v8:Length></v8:StringQualifiers>
          </Type>
        </Properties>
      </Attribute>
      <Attribute uuid="a2">
        <Properties>
          <Name>Поставщик</Name>
          <Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type>
        </Properties>
      </Attribute>
      <TabularSection uuid="t1">
        <Properties><Name>Цены</Name></Properties>
        <ChildObjects>
          <Attribute uuid="a3">
            <Properties>
              <Name>Цена</Name>
              <Type>
                <v8:Type>xs:decimal</v8:Type>
                <v8:NumberQualifiers><v8:Digits>15</v8:Digits><v8:FractionDigits>2</v8:FractionDigits></v8:NumberQualifiers>
              </Type>
            </Properties>
          </Attribute>
        </ChildObjects>
      </TabularSection>
    </ChildObjects>
  </Catalog>
</MetaDataObject>"#;

    #[test]
    fn реквизиты_читаются_плоским_списком() {
        let p = во_временный_файл("Товары.xml", СПРАВОЧНИК);
        let o = parse_object(&p, "Catalogs", "Catalogs/Товары.xml").unwrap();

        assert_eq!(o.name, "Товары");
        assert_eq!(o.synonym.as_deref(), Some("Товары"));
        // Три реквизита: два своих и один в табличной части.
        assert_eq!(o.attributes.len(), 3, "{:?}", o.attributes);
    }

    #[test]
    fn квалификаторы_сохраняются() {
        // То самое, что прежний инструмент теряет: длина строки и точность числа.
        let p = во_временный_файл("Товары2.xml", СПРАВОЧНИК);
        let o = parse_object(&p, "Catalogs", "Catalogs/Товары.xml").unwrap();

        let арт = o.attributes.iter().find(|a| a.name == "Артикул").unwrap();
        assert_eq!(арт.types, vec!["String"]);
        assert_eq!(арт.length, Some(25), "длина строки потеряна");

        let цена = o.attributes.iter().find(|a| a.name == "Цена").unwrap();
        assert_eq!(цена.types, vec!["Number"]);
        assert_eq!(цена.precision, Some(15));
        assert_eq!(цена.scale, Some(2));
    }

    #[test]
    fn реквизит_табличной_части_помечен_и_назван() {
        let p = во_временный_файл("Товары3.xml", СПРАВОЧНИК);
        let o = parse_object(&p, "Catalogs", "Catalogs/Товары.xml").unwrap();

        let цена = o.attributes.iter().find(|a| a.name == "Цена").unwrap();
        assert_eq!(цена.kind, "ts_attribute");
        assert_eq!(цена.ts_name.as_deref(), Some("Цены"));

        // А свой реквизит объекта табличной частью не помечен.
        let арт = o.attributes.iter().find(|a| a.name == "Артикул").unwrap();
        assert_eq!(арт.kind, "attribute");
        assert_eq!(арт.ts_name, None);
    }

    #[test]
    fn ссылочный_тип_без_префикса_пространства_имён() {
        let p = во_временный_файл("Товары4.xml", СПРАВОЧНИК);
        let o = parse_object(&p, "Catalogs", "Catalogs/Товары.xml").unwrap();
        let пост = o.attributes.iter().find(|a| a.name == "Поставщик").unwrap();
        assert_eq!(пост.types, vec!["CatalogRef.Контрагенты"]);
    }

    #[test]
    fn определяемый_тип_нормализуется_к_общему_виду() {
        // Дефект, пойманный 28.08.2026 при сборке ссылок: у нас лежало
        // `CatalogObject.X`, у прежнего инструмента `Catalog.X` — и разошлись 398
        // определяемых типов из 481. Прошлая сверка этого не видела,
        // потому что сравнивала ИМЕНА типов, а не их состав.
        let xml = r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <DefinedType>
    <Properties>
      <Name>Т</Name>
      <Type>
        <v8:Type>cfg:DocumentRef.РасходТовара</v8:Type>
        <v8:Type>cfg:ConstantValueManager.ВестиУчет</v8:Type>
        <v8:Type>cfg:BusinessProcessRoutePointRef.Задание</v8:Type>
      </Type>
    </Properties>
  </DefinedType>
</MetaDataObject>"#;
        let p = во_временный_файл("DefinedTypeNorm.xml", xml);
        let o = parse_object(&p, "DefinedTypes", "DefinedTypes/Т.xml").unwrap();
        assert_eq!(o.type_refs[0], "Document.РасходТовара");
        // Составной суффикс снимается целиком: `Manager` в одиночку оставил
        // бы несуществующий вид `ConstantValue`.
        assert_eq!(o.type_refs[1], "Constant.ВестиУчет");
        // А это НЕ трогается: `BusinessProcessRoutePoint` — не объект
        // метаданных, и прежний инструмент такое значение оставляет как есть.
        assert_eq!(o.type_refs[2], "BusinessProcessRoutePointRef.Задание");
    }

    #[test]
    fn параметр_команды_читается_и_из_typeset() {
        // Дефект того же захода: `in_type` открывался только тегом `Type`,
        // а определяемый тип в параметре команды записан через `TypeSet` —
        // терялось 20 ссылок из 610, причём молча.
        let xml = r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <CommonCommand>
    <Properties>
      <Name>ЗадачиПоПредмету</Name>
      <CommandParameterType>
        <v8:TypeSet>cfg:DefinedType.ПредметЗадачи</v8:TypeSet>
        <v8:Type>cfg:DocumentRef.Заказ</v8:Type>
      </CommandParameterType>
    </Properties>
  </CommonCommand>
</MetaDataObject>"#;
        let p = во_временный_файл("CmdParam.xml", xml);
        let o = parse_object(&p, "CommonCommands", "CommonCommands/З.xml").unwrap();
        assert_eq!(o.command_parameter_type.len(), 2);
        assert!(o
            .command_parameter_type
            .contains(&"DefinedType.ПредметЗадачи".to_string()));
        // Параметр команды не попадает в состав объекта: это разные вопросы —
        // «куда команда применяется» против «из чего объект состоит».
        assert!(o.type_refs.is_empty());
    }

    #[test]
    fn ввод_на_основании_и_владельцы_читаются() {
        let xml = r#"<MetaDataObject xmlns:xr="http://v8.1c.ru/8.3/xcf/readable">
  <Catalog>
    <Properties>
      <Name>БанковскиеСчета</Name>
      <Owners>
        <xr:Item xsi:type="xr:MDObjectRef">Catalog.Контрагенты</xr:Item>
        <xr:Item xsi:type="xr:MDObjectRef">Catalog.Организации</xr:Item>
      </Owners>
      <BasedOn>
        <xr:Item xsi:type="xr:MDObjectRef">Document.Счет</xr:Item>
      </BasedOn>
      <DefaultObjectForm>Catalog.БанковскиеСчета.Form.ФормаЭлемента</DefaultObjectForm>
      <DefaultListForm>Catalog.БанковскиеСчета.Form.ФормаСписка</DefaultListForm>
    </Properties>
  </Catalog>
</MetaDataObject>"#;
        let p = во_временный_файл("Owners.xml", xml);
        let o = parse_object(&p, "Catalogs", "Catalogs/БанковскиеСчета.xml").unwrap();
        assert_eq!(o.owners.len(), 2);
        assert_eq!(o.based_on, vec!["Document.Счет"]);
        assert_eq!(
            o.default_list_form.as_deref(),
            Some("Catalog.БанковскиеСчета.Form.ФормаСписка")
        );
        // Владельцы и основания не попадают в состав подсистемы: тег `Item`
        // одинаков, различает их только окружающая секция.
        assert!(o.content.is_empty());
    }

    #[test]
    fn тип_через_typeset_не_теряется() {
        // Дефект, пойманный сверкой 28.08.2026: тип задаётся ссылкой на
        // определяемый тип — `<v8:TypeSet>`, а не `<v8:Type>`. На БП таких
        // 2 862 вхождения; обработка только `Type` давала пустой список,
        // и «тип не разобрали» становилось неотличимо от «типа нет».
        let xml = r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <Document>
    <Properties><Name>АвансовыйОтчет</Name></Properties>
    <ChildObjects>
      <Attribute uuid="t-attribute">
        <Properties>
          <Name>Содержание</Name>
          <Type><v8:TypeSet>cfg:DefinedType.ОписаниеПокупки</v8:TypeSet></Type>
        </Properties>
      </Attribute>
    </ChildObjects>
  </Document>
</MetaDataObject>"#;
        let p = во_временный_файл("TypeSet.xml", xml);
        let o = parse_object(&p, "Documents", "Documents/АвансовыйОтчет.xml").unwrap();
        let a = &o.attributes[0];
        assert_eq!(
            a.types,
            vec!["DefinedType.ОписаниеПокупки"],
            "TypeSet потерян"
        );
    }

    #[test]
    fn одноимённые_реквизиты_разных_табличных_частей_различимы() {
        // Один и тот же реквизит в двух ТЧ имеет РАЗНЫЕ типы. Если не хранить
        // ts_name, четыре честные записи схлопываются в одну — именно так
        // 28.08.2026 сверка показала несуществующее расхождение типов.
        let xml = r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <Document>
    <Properties><Name>ВводНачальныхОстатков</Name></Properties>
    <ChildObjects>
      <TabularSection uuid="t-tabularsection">
        <Properties><Name>ОС</Name></Properties>
        <ChildObjects>
          <Attribute uuid="t-attribute"><Properties><Name>Способ</Name>
            <Type><v8:Type>cfg:EnumRef.СпособыОС</v8:Type></Type></Properties></Attribute>
        </ChildObjects>
      </TabularSection>
      <TabularSection uuid="t-tabularsection">
        <Properties><Name>НМА</Name></Properties>
        <ChildObjects>
          <Attribute uuid="t-attribute"><Properties><Name>Способ</Name>
            <Type><v8:Type>cfg:EnumRef.СпособыНМА</v8:Type></Type></Properties></Attribute>
        </ChildObjects>
      </TabularSection>
    </ChildObjects>
  </Document>
</MetaDataObject>"#;
        let p = во_временный_файл("ДвеТЧ.xml", xml);
        let o = parse_object(&p, "Documents", "Documents/ВводНачальныхОстатков.xml").unwrap();

        assert_eq!(o.attributes.len(), 2);
        let ос = o
            .attributes
            .iter()
            .find(|a| a.ts_name.as_deref() == Some("ОС"))
            .expect("реквизит ТЧ ОС");
        let нма = o
            .attributes
            .iter()
            .find(|a| a.ts_name.as_deref() == Some("НМА"))
            .expect("реквизит ТЧ НМА");
        assert_eq!(ос.types, vec!["EnumRef.СпособыОС"]);
        assert_eq!(нма.types, vec!["EnumRef.СпособыНМА"]);
    }

    #[test]
    fn реквизит_объекта_после_табличной_части_не_помечен_как_её_реквизит() {
        // Проверка того, что счётчик вложенности возвращается: реквизит,
        // объявленный ПОСЛЕ табличной части, принадлежит объекту.
        let xml = r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <Document>
    <Properties><Name>Док</Name></Properties>
    <ChildObjects>
      <TabularSection uuid="t-tabularsection">
        <Properties><Name>Товары</Name></Properties>
        <ChildObjects>
          <Attribute uuid="t-attribute"><Properties><Name>Цена</Name></Properties></Attribute>
        </ChildObjects>
      </TabularSection>
      <Attribute uuid="t-attribute"><Properties><Name>Комментарий</Name></Properties></Attribute>
    </ChildObjects>
  </Document>
</MetaDataObject>"#;
        let p = во_временный_файл("ПослеТЧ.xml", xml);
        let o = parse_object(&p, "Documents", "Documents/Док.xml").unwrap();
        let к = o
            .attributes
            .iter()
            .find(|a| a.name == "Комментарий")
            .unwrap();
        assert_eq!(
            к.kind, "attribute",
            "реквизит объекта записан как реквизит ТЧ"
        );
        assert_eq!(к.ts_name, None);
    }

    #[test]
    fn список_категорий_включает_объекты_без_реквизитов() {
        // Синоним есть у ВСЯКОГО объекта. Короткий список категорий терял
        // 7 442 синонима из 13 497 — молча, при внешне заполненной таблице.
        for c in ["CommonModules", "Roles", "CommonCommands", "XDTOPackages"] {
            assert!(CATEGORIES.contains(&c), "категория {c} потеряна");
        }
    }

    #[test]
    fn пустое_свойство_не_обрывает_разбор_измерения() {
        // Дефект 28.08.2026: `<AccountingFlag/>` — свойство измерения, а не
        // подчинённый объект. Считая его объектом, разбор терял всё, что шло
        // после: на регистре «Хозрасчетный» 8 подчинённых из 11, молча.
        let xml = r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <AccountingRegister>
    <Properties><Name>Хозрасчетный</Name></Properties>
    <ChildObjects>
      <Dimension uuid="d1">
        <Properties>
          <Name>Организация</Name>
          <Type><v8:Type>cfg:CatalogRef.Организации</v8:Type></Type>
          <AccountingFlag/>
        </Properties>
      </Dimension>
      <Dimension uuid="d2">
        <Properties>
          <Name>Валюта</Name>
          <Type><v8:Type>cfg:CatalogRef.Валюты</v8:Type></Type>
        </Properties>
      </Dimension>
      <Resource uuid="r1">
        <Properties><Name>Сумма</Name></Properties>
      </Resource>
    </ChildObjects>
  </AccountingRegister>
</MetaDataObject>"#;
        let p = во_временный_файл("Хозрасчетный.xml", xml);
        let o = parse_object(
            &p,
            "AccountingRegisters",
            "AccountingRegisters/Хозрасчетный.xml",
        )
        .unwrap();

        let имена: Vec<&str> = o.attributes.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            имена,
            vec!["Организация", "Валюта", "Сумма"],
            "разбор оборван"
        );
        assert_eq!(o.attributes[1].kind, "dimension");
        assert_eq!(o.attributes[2].kind, "resource");
    }

    #[test]
    fn предопределённые_читаются_с_кодом() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<PredefinedData xmlns="http://v8.1c.ru/8.3/xcf/predef">
  <Item id="1"><Name>Основной</Name><Code>000001</Code><Description>Основной склад</Description><IsFolder>false</IsFolder></Item>
  <Item id="2"><Name>Группа</Name><Code>000002</Code><Description>Группа</Description><IsFolder>true</IsFolder></Item>
</PredefinedData>"#;
        let p = во_временный_файл("Predefined.xml", xml);
        let items = parse_predefined(&p);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "Основной");
        assert_eq!(items[0].code.as_deref(), Some("000001"));
        assert!(!items[0].is_folder);
        assert!(items[1].is_folder);
    }

    #[test]
    fn отсутствие_файла_предопределённых_не_ошибка() {
        // Большинство объектов предопределённых не имеют: пустой список — норма.
        let p = std::env::temp_dir().join("gyrfalcon-нет-такого-файла.xml");
        assert!(parse_predefined(&p).is_empty());
    }

    #[test]
    fn объект_без_имени_отвергается() {
        // Правило заглушек: пустое имя — признак неразобранного файла,
        // а не объекта без имени. Пустышка в индексе стала бы «фактом».
        let p = во_временный_файл(
            "Битый.xml",
            "<MetaDataObject><Catalog/></MetaDataObject>",
        );
        assert!(parse_object(&p, "Catalogs", "Catalogs/Битый.xml").is_none());
    }
}
