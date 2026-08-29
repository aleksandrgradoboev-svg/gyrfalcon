//! Визуальная часть: карта конфигурации и движения по регистрам.
//!
//! # Почему свой HTTP и SVG, а не готовый стек
//!
//! У образца это отдельный фронтенд-проект (React + Three.js + сборка Vite).
//! Красиво, но тянет Node в требования разработки и сотни пакетов
//! в зависимости — ради страницы, которую открывают, чтобы посмотреть
//! на конфигурацию.
//!
//! Здесь страница **вшита в бинарь** (та же логика, что со скиллом:
//! бинарь самодостаточен) и рисуется SVG. Интерактив от этого не страдает:
//! зум, перетаскивание полотна и узлов, подсветка связей, фильтры и поиск —
//! всё это чистый DOM. Теряется только 3D, а для двудольного графа
//! документ→регистр оно скорее мешает: связи перекрываются и читаются хуже,
//! чем на плоскости.
//!
//! # Что показывается
//!
//! 1. **Движения**: документы → регистры. Кто куда пишет, где хабы,
//!    какие регистры никто не заполняет.
//! 2. **Карта конфигурации**: подсистемы кругами, площадь по числу объектов.
//! 3. **Расширения** накладкой: что перехвачено и чем.
//!
//! # Раскладка считается на сервере
//!
//! Приём взят у образца — клиент получает готовые координаты и рисует.
//! Причина та же: раскладка это работа над данными, которые и так лежат
//! на сервере, а гонять 2 108 рёбер в браузер, чтобы он их разложил, —
//! лишний круг.

use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// Страница целиком: вшита в бинарь, как скилл.
pub const PAGE: &str = include_str!("ui.html");

/// Домашняя подсистема объекта.
///
/// Объект бывает сразу в нескольких подсистемах, и это не ошибка: крупные
/// («БухгалтерияПредприятияПодсистемы» — 2 418 объектов) включают почти всё
/// подряд и кластера не задают. Поэтому домашней считается **наименьшая**
/// подсистема, в которую объект входит: она несёт больше смысла о том,
/// к какому участку учёта он относится.
fn домашние_подсистемы(
    conn: &Connection,
) -> Result<HashMap<String, String>, String> {
    let mut st = conn
        .prepare(
            "WITH размер AS (
                 SELECT subsystem_name, count(*) n FROM subsystem_content GROUP BY 1
             )
             SELECT s.object_ref, s.subsystem_name,
                    row_number() OVER (PARTITION BY s.object_ref ORDER BY r.n) rn
             FROM subsystem_content s JOIN размер r ON r.subsystem_name = s.subsystem_name",
        )
        .map_err(|e| e.to_string())?;
    let mut m = HashMap::new();
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for r in rows.flatten() {
        if r.2 == 1 {
            // Ключ — короткое имя объекта: в движениях оно без префикса вида.
            let имя = r.0.rsplit('.').next().unwrap_or(&r.0).to_string();
            m.insert(имя, r.1);
        }
    }
    Ok(m)
}

/// Данные для отрисовки движений документ→регистр.
///
/// # Раскладка: сначала структура, потом силы
///
/// Приём взят у образца, и порядок в нём главное. Их комментарий: сперва
/// узлы ставятся **на кольцо по ключу кластера**, и только потом короткая
/// силовая релаксация правит положение **внутри** группы. Чистая силовая
/// раскладка на двух тысячах рёбер даёт кашу — проверено собственной первой
/// редакцией этой страницы.
///
/// Ключ кластера у нас лучше, чем у образца: у них каталог файловой системы,
/// у нас **подсистема** — то есть деление, объявленное разработчиком
/// конфигурации. Отсюда и польза картинки: связи внутри подсистемы идут
/// короткими дугами по краю, а хорды через центр показывают, где учёт
/// перетекает между участками.
pub fn movements_data(conn: &Connection, limit: usize) -> Result<Value, String> {
    let mut st = conn
        .prepare(
            "SELECT document_name, register_name, source
             FROM register_movements ORDER BY document_name, register_name",
        )
        .map_err(|e| e.to_string())?;
    let рёбра: Vec<(String, String, String)> = st
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();

    // Вес узла = число его связей. По нему же отбираем, что показать:
    // на большой конфигурации всё сразу — каша, а «первые N по алфавиту»
    // отрезали бы как раз главное.
    let mut вес_док: HashMap<&str, usize> = HashMap::new();
    let mut вес_рег: HashMap<&str, usize> = HashMap::new();
    for (d, r, _) in &рёбра {
        *вес_док.entry(d.as_str()).or_default() += 1;
        *вес_рег.entry(r.as_str()).or_default() += 1;
    }

    let отобрать = |m: &HashMap<&str, usize>| -> Vec<(String, usize)> {
        let mut v: Vec<(String, usize)> = m.iter().map(|(k, n)| (k.to_string(), *n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(limit);
        v
    };
    let доки = отобрать(&вес_док);
    let реги = отобрать(&вес_рег);

    let имена_док: HashSet<&str> = доки.iter().map(|(n, _)| n.as_str()).collect();
    let имена_рег: HashSet<&str> = реги.iter().map(|(n, _)| n.as_str()).collect();

    let видимые: Vec<&(String, String, String)> = рёбра
        .iter()
        .filter(|(d, r, _)| имена_док.contains(d.as_str()) && имена_рег.contains(r.as_str()))
        .collect();

    // Кластер узла — его домашняя подсистема. Без неё узел попадает в группу
    // «вне подсистем»: это честнее, чем приписать его к соседней по алфавиту.
    let дом = домашние_подсистемы(conn).unwrap_or_default();
    const БЕЗ: &str = "— вне подсистем —";
    let кластер =
        |имя: &str| -> String { дом.get(имя).cloned().unwrap_or_else(|| БЕЗ.to_string()) };

    // Порядок узлов на кольце задаётся кластером: сперва группируем, потом
    // внутри группы сортируем по весу. Это и есть «сначала структура»
    // из образца — без него силовая релаксация на клиенте даст кашу.
    let собрать = |v: &[(String, usize)], вид: &str| -> Vec<Value> {
        let mut список: Vec<(String, String, usize)> =
            v.iter().map(|(n, w)| (кластер(n), n.clone(), *w)).collect();
        список.sort_by(|a, b| a.0.cmp(&b.0).then(b.2.cmp(&a.2)).then(a.1.cmp(&b.1)));
        список
            .into_iter()
            .map(|(k, n, w)| json!({"name": n, "weight": w, "cluster": k, "kind": вид}))
            .collect()
    };

    let узлы_док = собрать(&доки, "document");
    let узлы_рег = собрать(&реги, "register");

    // Список кластеров с их размером — клиент отводит каждому сектор кольца
    // пропорционально числу узлов.
    let mut размер: HashMap<String, usize> = HashMap::new();
    for u in узлы_док.iter().chain(узлы_рег.iter()) {
        *размер
            .entry(u["cluster"].as_str().unwrap_or(БЕЗ).to_string())
            .or_default() += 1;
    }
    let mut кластеры: Vec<(String, usize)> = размер.into_iter().collect();
    // «Вне подсистем» всегда последним: это не участок учёта, а остаток.
    кластеры.sort_by(|a, b| {
        (a.0 == БЕЗ)
            .cmp(&(b.0 == БЕЗ))
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(&b.0))
    });

    Ok(json!({
        "kind": "movements",
        "documents": узлы_док,
        "registers": узлы_рег,
        "clusters": кластеры.iter()
            .map(|(n, s)| json!({"name": n, "size": s}))
            .collect::<Vec<_>>(),
        "links": видимые.iter()
            .map(|(d, r, s)| json!({"from": d, "to": r, "source": s}))
            .collect::<Vec<_>>(),
        // Усечение называется вслух: молча обрезанная картина читается как полная.
        "total_links": рёбра.len(),
        "total_documents": вес_док.len(),
        "total_registers": вес_рег.len(),
        "shown_limit": limit,
    }))
}

/// Путь вложенности подсистемы, разобранный из пути файла.
///
/// Отдельной колонки `parent` в схеме нет, но данные не потеряны: вложенность
/// лежит в пути XML-файла, где уровни разделены каталогами `Subsystems`:
///
/// ```text
/// Subsystems/Администрирование/Subsystems/КонтрольРаботыПользователей.xml
///            └─ родитель ────┘            └─ дочерняя ─────────────────┘
/// ```
///
/// На БП это 588 подсистем и **пять уровней** вложенности — глубина,
/// которую плоский список теряет целиком.
fn путь_подсистемы(файл: &str) -> Vec<String> {
    файл
        .trim_end_matches(".xml")
        .split('/')
        .filter(|ч| !ч.is_empty() && *ч != "Subsystems")
        .map(str::to_string)
        .collect()
}

/// Карта конфигурации: подсистемы, их состав и **вложенность**.
pub fn subsystems_data(conn: &Connection, limit: usize) -> Result<Value, String> {
    let mut st = conn
        .prepare(
            "SELECT subsystem_name, subsystem_synonym, count(*) AS n,
                    sum(CASE WHEN object_ref LIKE 'Document.%' THEN 1 ELSE 0 END),
                    sum(CASE WHEN object_ref LIKE 'Catalog.%' THEN 1 ELSE 0 END),
                    sum(CASE WHEN object_ref LIKE '%Register.%' THEN 1 ELSE 0 END),
                    sum(CASE WHEN object_ref LIKE 'Report.%' THEN 1 ELSE 0 END),
                    min(file)
             FROM subsystem_content GROUP BY subsystem_name, subsystem_synonym
             ORDER BY n DESC LIMIT ?1",
        )
        .map_err(|e| e.to_string())?;
    let узлы: Vec<Value> = st
        .query_map([limit as i64], |r| {
            let файл: String = r.get::<_, Option<String>>(7)?.unwrap_or_default();
            let путь = путь_подсистемы(&файл);
            Ok(json!({
                "name": r.get::<_, String>(0)?,
                "synonym": r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                "total": r.get::<_, i64>(2)?,
                "documents": r.get::<_, i64>(3)?,
                "catalogs": r.get::<_, i64>(4)?,
                "registers": r.get::<_, i64>(5)?,
                "reports": r.get::<_, i64>(6)?,
                "path": путь,
                "depth": путь.len(),
                "parent": if путь.len() > 1 { json!(путь[путь.len()-2]) } else { Value::Null },
            }))
        })
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();

    let всего: i64 = conn
        .query_row(
            "SELECT count(DISTINCT subsystem_name) FROM subsystem_content",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    Ok(json!({
        "kind": "subsystems",
        "nodes": узлы,
        "total_subsystems": всего,
        "shown_limit": limit,
    }))
}

/// Перехваты расширений — накладка на карту.
///
/// # Почему адрес выдаётся, а не остаётся в индексе
///
/// «Перехват найден **с адресом** — модуль, строка, вид» записано в
/// ось приёмки, на которой адрес перехвата и есть результат. Первая редакция
/// брала из тринадцати колонок пять и адрес не отдавала вовсе: картинка
/// выглядела рабочей, а доказать на ней заявленное преимущество было нечем.
/// Тот же жанр дефекта, что четыре найденных живым вызовом в вехе 5 —
/// данные в индексе есть, наружу не идут.
///
/// Адресов **два**, и путать их нельзя:
///
/// - `target_*` — где перехватывают: модуль и строка в основной конфигурации;
/// - `ext_*` — где лежит сам перехватчик: модуль и строка в расширении.
///
/// Разработчику нужны оба: первый отвечает «что подменили», второй — «куда
/// идти читать подмену».
pub fn overrides_data(conn: &Connection) -> Result<Value, String> {
    let mut st = conn
        .prepare(
            "SELECT extension_name, extension_purpose, object_name, target_method, annotation,
                    source_path, target_method_line, extension_method, ext_module_path, ext_line
             FROM extension_overrides ORDER BY extension_name, object_name",
        )
        .map_err(|e| e.to_string())?;
    let строки: Vec<Value> = st
        .query_map([], |r| {
            Ok(json!({
                "extension": r.get::<_, String>(0)?,
                "purpose": r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                "object": r.get::<_, String>(2)?,
                "method": r.get::<_, String>(3)?,
                "annotation": r.get::<_, String>(4)?,
                // Адрес цели: что именно подменяется в основной конфигурации.
                "target_path": r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                "target_line": r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                // Адрес самого перехватчика: куда идти читать подмену.
                "ext_method": r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                "ext_path": r.get::<_, Option<String>>(8)?.unwrap_or_default(),
                "ext_line": r.get::<_, Option<i64>>(9)?.unwrap_or(0),
            }))
        })
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    Ok(json!({"kind": "overrides", "items": строки, "count": строки.len()}))
}

/// Сводка индекса — шапка страницы.
pub fn summary_data(conn: &Connection) -> Result<Value, String> {
    let один = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(-1) };
    let путь: String = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'source_path'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "—".into());
    let (всего, разрешено) = (
        один("SELECT count(*) FROM calls"),
        один("SELECT count(*) FROM calls WHERE callee_key IS NOT NULL"),
    );
    Ok(json!({
        "source": путь,
        "modules": один("SELECT count(*) FROM modules"),
        "methods": один("SELECT count(*) FROM methods"),
        "calls": всего,
        "calls_resolved": разрешено,
        "resolved_pct": if всего > 0 {
            (разрешено as f64 / всего as f64 * 1000.0).round() / 10.0
        } else {
            0.0
        },
        "objects": один("SELECT count(DISTINCT object_name) FROM object_attributes"),
        "movements": один("SELECT count(*) FROM register_movements"),
        "subsystems": один("SELECT count(DISTINCT subsystem_name) FROM subsystem_content"),
        "overrides": один("SELECT count(*) FROM extension_overrides"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn база() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE register_movements (document_name TEXT, register_name TEXT, source TEXT);
             INSERT INTO register_movements VALUES
               ('ОперацияБух','Хозрасчетный','declared'),
               ('ОперацияБух','НДСПродажи','declared'),
               ('Реализация','Хозрасчетный','declared'),
               ('Реализация','НДСПродажи','code'),
               ('Мелкий','Одинокий','declared');
             CREATE TABLE subsystem_content (subsystem_name TEXT, subsystem_synonym TEXT,
               object_ref TEXT, file TEXT);
             INSERT INTO subsystem_content VALUES
               ('Учет','Учёт','Document.Реализация','Subsystems/Учет.xml'),
               ('Учет','Учёт','Catalog.Номенклатура','Subsystems/Учет.xml'),
               ('Мелкая','Мелкая','Report.Оборотка','Subsystems/Учет/Subsystems/Мелкая.xml');
             ",
        )
        .unwrap();
        // Таблица перехватов создаётся НАСТОЯЩИМ DDL индексатора, а не
        // усечённым списком колонок «сколько нужно тесту». Прежняя фикстура
        // объявляла пять колонок из тринадцати, и запрос, читающий адрес
        // перехвата, падал в тесте, работая на живом индексе. Это тот же
        // жанр дефекта, что четыре найденных живым вызовом в вехе 5:
        // **схема, написанная по представлению о ней**. Здесь он закрыт
        // структурно — фикстура не может разойтись с DDL, потому что это
        // один и тот же текст.
        c.execute_batch(gyrfalcon_index::ddl::SCHEMA_EXTENSIONS)
            .unwrap();
        c.execute_batch(
            "INSERT INTO extension_overrides
               (object_name, source_path, target_method, target_method_line, annotation,
                extension_name, extension_purpose, extension_method,
                extension_root, ext_module_path, ext_line)
             VALUES
               ('Документ.Реализация','Documents/Реализация/Ext/ObjectModule.bsl',
                'ОбработкаПроведения', 42, 'После',
                'НашеРасширение','Настройка','Расш1_ОбработкаПроведения',
                'C:/ext/НашеРасширение','Documents/Реализация/Ext/ObjectModule.bsl', 7);",
        )
        .unwrap();
        c
    }

    #[test]
    fn движения_отдают_узлы_и_связи() {
        let c = база();
        let v = movements_data(&c, 10).unwrap();
        assert_eq!(v["total_links"], 5);
        assert_eq!(v["total_documents"], 3);
        // Порядок теперь СПЕРВА по кластеру, потом по весу: узлы одной
        // подсистемы идут подряд, чтобы на кольце оказаться рядом. Поэтому
        // «самый тяжёлый первым» больше не выполняется глобально — проверяем,
        // что вес доехал и кластер проставлен.
        let док = v["documents"].as_array().unwrap();
        let оп = док
            .iter()
            .find(|d| d["name"] == "ОперацияБух")
            .expect("ОперацияБух не найден");
        assert_eq!(оп["weight"], 2);
        assert!(оп["cluster"].is_string(), "кластер обязан быть проставлен");
    }

    #[test]
    fn усечение_названо_вслух() {
        // Молча обрезанная картина читается как полная — и вывод «регистров
        // всего два» будет неверным.
        let c = база();
        let v = movements_data(&c, 1).unwrap();
        assert_eq!(v["total_documents"], 3, "полное число обязано остаться");
        assert_eq!(v["documents"].as_array().unwrap().len(), 1);
        assert_eq!(v["shown_limit"], 1);
    }

    #[test]
    fn связи_отбираются_по_видимым_узлам() {
        // Ребро на узел, который не показан, рисовать некуда: оно бы висело
        // в пустоте и выглядело как связь с чем-то невидимым.
        let c = база();
        let v = movements_data(&c, 1).unwrap();
        let links = v["links"].as_array().unwrap();
        for l in links {
            assert_eq!(l["from"], "ОперацияБух");
        }
    }

    #[test]
    fn подсистемы_считают_состав_по_категориям() {
        let c = база();
        let v = subsystems_data(&c, 10).unwrap();
        assert_eq!(v["nodes"][0]["name"], "Учет");
        assert_eq!(v["nodes"][0]["total"], 2);
        assert_eq!(v["nodes"][0]["documents"], 1);
        assert_eq!(v["nodes"][0]["catalogs"], 1);
        // Вложенность разбирается из пути файла — отдельной колонки нет.
        assert!(
            v["nodes"][0]["path"].is_array(),
            "путь вложенности не отдан"
        );
    }

    #[test]
    fn перехваты_отдаются_с_аннотацией() {
        let c = база();
        let v = overrides_data(&c).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["items"][0]["annotation"], "После");
    }

    /// «Перехват найден **с адресом** — модуль, строка, вид» — ось приёмки
    /// приёмки: адрес перехвата и есть результат. Пока адрес не
    /// проверен тестом, он может тихо пропасть из выдачи (и однажды уже
    /// пропал: первая редакция страницы брала пять колонок из тринадцати).
    #[test]
    fn у_перехвата_есть_адрес_обеих_сторон() {
        let c = база();
        let v = overrides_data(&c).unwrap();
        let i = &v["items"][0];
        // Где перехватывают — в основной конфигурации.
        assert_eq!(
            i["target_path"],
            "Documents/Реализация/Ext/ObjectModule.bsl"
        );
        assert_eq!(i["target_line"], 42);
        // Где лежит сам перехватчик — в расширении.
        assert_eq!(i["ext_method"], "Расш1_ОбработкаПроведения");
        assert_eq!(i["ext_line"], 7);
    }

    #[test]
    fn страница_вшита_и_самодостаточна() {
        assert!(PAGE.len() > 2000, "страница подозрительно короткая");
        // Ни одной внешней загрузки: страница обязана работать без сети.
        //
        // Проверяется не подстрока «http://» — она законно встречается в
        // пространстве имён SVG (`http://www.w3.org/2000/svg`), которое ничего
        // не загружает. Проверяются теги и вызовы, которые действительно ходят
        // в сеть.
        for опасное in [
            "<script src=",
            "<link rel=\"stylesheet\" href=\"http",
            "@import",
            "cdn.",
            "unpkg",
            "jsdelivr",
            "googleapis",
        ] {
            assert!(
                !PAGE.contains(опасное),
                "страница тянет что-то из сети: {опасное}"
            );
        }
    }
}
