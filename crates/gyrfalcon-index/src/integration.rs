//! Интеграция и движения: движения регистров, XDTO, web- и HTTP-сервисы.
//!
//! Четыре таблицы, закрывающие остаток «мелкого» из вехи 3. Общее у них —
//! каждая отвечает на вопрос вида «куда это ведёт»: документ в регистр,
//! пакет в типы, сервис в операции.
//!
//! # Два места, где мы намеренно полнее прежнего инструмента
//!
//! **Движения.** Прежний инструмент собирает их только из кода. Объявленный состав
//! `<RegisterRecords>` в XML документа он не читает: замер 28.08.2026 дал
//! 2 108 объявленных движений у 236 документов против 245 пар у 135 у прежнего инструмента.
//! Мы берём объединение и помечаем источник (`declared` против кодовых
//! классов), поэтому паритет по числу строк здесь недостижим и не нужен.
//!
//! **XDTO.** У прежнего инструмента `types_json` пуст во всех 434 строках. Схема пакета лежит
//! не в `XDTOPackages/<Имя>.xml`, а в спутнике `<Имя>/Ext/Package.bin` — файл с
//! расширением `.bin`, внутри которого обычный XML. Замер по всем 434 пакетам:
//! 16 389 `objectType`, 6 749 `valueType`, 110 988 `property`.
//!
//! # Откуда взяты поля
//!
//! Столбцы — с индекса прежнего инструмента (`sqlite_master`), значения сверены с живой
//! выгрузкой БП 28.08.2026. Формы тегов (`RegisterRecords/xr:Item`,
//! `Operation/Parameter`, `URLTemplate/Method`, `package/objectType`) прочитаны
//! в файлах, а не восстановлены по памяти об устройстве 1С.

use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::Path;

/// Одно движение: документ пишет в регистр.
#[derive(Debug, Clone)]
pub struct RegisterMovement {
    pub document_name: String,
    /// Имя регистра без префикса вида: `ПрочиеРасчеты`, а не
    /// `AccumulationRegister.ПрочиеРасчеты` — форма прежнего инструмента.
    pub register_name: String,
    /// `declared` — объявление в метаданных документа; кодовые классы — как у прежнего инструмента.
    pub source: &'static str,
    pub file: String,
}

/// Тип внутри пакета XDTO.
#[derive(Debug, Clone)]
pub struct XdtoType {
    /// `objectType` или `valueType`.
    pub kind: &'static str,
    pub name: String,
    /// База типа: атрибут `base`.
    pub base: Option<String>,
    /// Свойства объектного типа; у типов-значений пусто.
    pub properties: Vec<XdtoProperty>,
}

/// Свойство типа XDTO.
#[derive(Debug, Clone)]
pub struct XdtoProperty {
    pub name: String,
    pub type_ref: Option<String>,
    pub lower_bound: Option<String>,
    pub upper_bound: Option<String>,
}

/// Пакет XDTO.
#[derive(Debug, Clone)]
pub struct XdtoPackage {
    pub name: String,
    pub namespace: String,
    pub types: Vec<XdtoType>,
    pub file: String,
}

/// Операция web-сервиса.
#[derive(Debug, Clone)]
pub struct WebOperation {
    pub name: String,
    pub return_type: Option<String>,
    pub procedure_name: Option<String>,
    pub params: Vec<String>,
}

/// Web-сервис.
#[derive(Debug, Clone)]
pub struct WebService {
    pub name: String,
    pub namespace: String,
    pub operations: Vec<WebOperation>,
    pub file: String,
}

/// Метод шаблона URL.
#[derive(Debug, Clone)]
pub struct HttpMethod {
    pub name: String,
    pub http_method: Option<String>,
    pub handler: Option<String>,
}

/// Шаблон URL HTTP-сервиса.
#[derive(Debug, Clone)]
pub struct HttpTemplate {
    pub name: String,
    pub template: Option<String>,
    pub methods: Vec<HttpMethod>,
}

/// HTTP-сервис.
#[derive(Debug, Clone)]
pub struct HttpService {
    pub name: String,
    pub root_url: String,
    pub templates: Vec<HttpTemplate>,
    pub file: String,
}

fn read_text(path: &Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
    Some(String::from_utf8_lossy(raw).into_owned())
}

fn reader_for(text: &str) -> Reader<&[u8]> {
    let mut reader = Reader::from_reader(text.as_bytes());
    let cfg = reader.config_mut();
    cfg.trim_text(true);
    cfg.check_end_names = false;
    reader
}

fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.rsplit(':').next().unwrap_or("").to_string()
}

/// Значение атрибута по локальному имени (префикс пространства имён снят).
fn attr(e: &quick_xml::events::BytesStart, want: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (local_name(a.key.as_ref()) == want)
            .then(|| String::from_utf8_lossy(a.value.as_ref()).into_owned())
    })
}

/// Убрать префикс вида у ссылки на объект: `AccumulationRegister.X` в `X`.
///
/// Именно так хранит прежний инструмент (`register_name` = `ПрочиеРасчеты`), и менять форму
/// значило бы разойтись с ним в данных ради своей эстетики.
fn strip_kind(t: &str) -> &str {
    t.rsplit('.').next().unwrap_or(t)
}

/// Пары (путь тегов от корня, текст).
///
/// Тот же приём, что в `meta2`: `<Name>` встречается на каждом уровне
/// вложенности, и различать вхождения можно только по пути, а не по имени тега.
fn tag_paths(text: &str) -> Vec<(Vec<String>, String)> {
    let mut out = Vec::new();
    let mut reader = reader_for(text);
    let mut stack: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => stack.push(local_name(e.name().as_ref())),
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Text(t)) => {
                let s = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                if !s.trim().is_empty() {
                    out.push((stack.clone(), s));
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Прямое свойство объекта: тег внутри `Properties` на заданной глубине.
fn prop_at(pairs: &[(Vec<String>, String)], depth: usize, tag: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(p, _)| {
            p.len() == depth
                && p.last().map(String::as_str) == Some(tag)
                && p[p.len() - 2] == "Properties"
        })
        .map(|(_, v)| v.clone())
}

/// Объявленные движения документа — из `<RegisterRecords>` его XML.
///
/// Форма проверена в выгрузке: список `<xr:Item xsi:type="xr:MDObjectRef">`
/// со значениями вида `AccumulationRegister.ПрочиеРасчеты` — та же, что у
/// состава подсистемы. Пустой `<RegisterRecords/>` даёт пустой вектор, и это
/// факт о документе, а не сбой разбора.
pub fn parse_declared_movements(path: &Path, document: &str, rel: &str) -> Vec<RegisterMovement> {
    let Some(text) = read_text(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut reader = reader_for(&text);
    let mut buf = Vec::new();
    let mut inside = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if local_name(e.name().as_ref()) == "RegisterRecords" {
                    inside = true;
                }
            }
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == "RegisterRecords" {
                    inside = false;
                }
            }
            Ok(Event::Text(t)) if inside => {
                let s = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                let s = s.trim();
                if !s.is_empty() {
                    out.push(RegisterMovement {
                        document_name: document.to_string(),
                        register_name: strip_kind(s).to_string(),
                        source: "declared",
                        file: rel.to_string(),
                    });
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Пакет XDTO: имя и пространство имён из `<Имя>.xml`, типы — из спутника
/// `<Имя>/Ext/Package.bin`.
///
/// Отсутствие спутника даёт пакет с пустым списком типов — внешне как у прежнего инструмента,
/// но по другой причине: у него пусты все, у нас только те, где файла нет.
pub fn parse_xdto_package(meta_path: &Path, bin_path: &Path, rel: &str) -> Option<XdtoPackage> {
    let text = read_text(meta_path)?;
    let pairs = tag_paths(&text);
    let name = prop_at(&pairs, 4, "Name")?;
    let namespace = prop_at(&pairs, 4, "Namespace").unwrap_or_default();
    let types = read_text(bin_path)
        .map(|t| parse_package_types(&t))
        .unwrap_or_default();
    Some(XdtoPackage {
        name,
        namespace,
        types,
        file: rel.to_string(),
    })
}

/// Разбор схемы пакета из `Package.bin`.
///
/// Внутри — XML с корнем `<package targetNamespace=…>`; `objectType` несёт
/// вложенные `<property>`, `valueType` — базовый тип. Свойства верхнего уровня
/// (прямо в `package`, вне типа) к составу типов не относятся и не пишутся:
/// именно поэтому нужен признак `in_type`, а не «последний виденный тип».
pub fn parse_package_types(text: &str) -> Vec<XdtoType> {
    let mut out: Vec<XdtoType> = Vec::new();
    let mut reader = reader_for(text);
    let mut buf = Vec::new();
    let mut in_type = false;
    loop {
        let ev = reader.read_event_into(&mut buf);
        match ev {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let empty = matches!(ev, Ok(Event::Empty(_)));
                match local_name(e.name().as_ref()).as_str() {
                    kind @ ("objectType" | "valueType") => {
                        out.push(XdtoType {
                            kind: if kind == "objectType" {
                                "objectType"
                            } else {
                                "valueType"
                            },
                            name: attr(e, "name").unwrap_or_default(),
                            base: attr(e, "base"),
                            properties: Vec::new(),
                        });
                        // Самозакрытый тип не содержит свойств и не открывает область.
                        in_type = !empty;
                    }
                    // Свойство вне типа — глобальное свойство пакета: в состав
                    // типов не идёт, поэтому guard, а не `if` внутри ветки.
                    "property" if in_type => {
                        if let Some(t) = out.last_mut() {
                            t.properties.push(XdtoProperty {
                                name: attr(e, "name").unwrap_or_default(),
                                type_ref: attr(e, "type"),
                                lower_bound: attr(e, "lowerBound"),
                                upper_bound: attr(e, "upperBound"),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let n = local_name(e.name().as_ref());
                if n == "objectType" || n == "valueType" {
                    in_type = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Web-сервис: операции с типом возврата, именем процедуры и параметрами.
pub fn parse_web_service(path: &Path, rel: &str) -> Option<WebService> {
    let text = read_text(path)?;
    let pairs = tag_paths(&text);
    let name = prop_at(&pairs, 4, "Name")?;
    let namespace = prop_at(&pairs, 4, "Namespace").unwrap_or_default();

    let mut operations: Vec<WebOperation> = Vec::new();
    let mut reader = reader_for(&text);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(e.name().as_ref());
                if n == "Operation" {
                    operations.push(WebOperation {
                        name: String::new(),
                        return_type: None,
                        procedure_name: None,
                        params: Vec::new(),
                    });
                }
                stack.push(n);
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Text(t)) => {
                let s = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                let s = s.trim().to_string();
                if s.is_empty() {
                    buf.clear();
                    continue;
                }
                let tag = stack.last().map(String::as_str).unwrap_or("");
                let in_props = stack.len() >= 2 && stack[stack.len() - 2] == "Properties";
                let in_param = stack.iter().any(|t| t == "Parameter");
                if let Some(op) = operations.last_mut() {
                    if in_props && !in_param {
                        match tag {
                            "Name" if op.name.is_empty() => op.name = s,
                            "XDTOReturningValueType" => op.return_type = Some(s),
                            "ProcedureName" => op.procedure_name = Some(s),
                            _ => {}
                        }
                    } else if in_props && in_param && tag == "Name" {
                        op.params.push(s);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    // Операция без имени — сбой разбора, а не факт о сервисе: не пишем.
    operations.retain(|o| !o.name.is_empty());
    Some(WebService {
        name,
        namespace,
        operations,
        file: rel.to_string(),
    })
}

/// HTTP-сервис: шаблоны URL и методы с обработчиками.
pub fn parse_http_service(path: &Path, rel: &str) -> Option<HttpService> {
    let text = read_text(path)?;
    let pairs = tag_paths(&text);
    let name = prop_at(&pairs, 4, "Name")?;
    let root_url = prop_at(&pairs, 4, "RootURL").unwrap_or_default();

    let mut templates: Vec<HttpTemplate> = Vec::new();
    let mut reader = reader_for(&text);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(e.name().as_ref());
                if n == "URLTemplate" {
                    templates.push(HttpTemplate {
                        name: String::new(),
                        template: None,
                        methods: Vec::new(),
                    });
                } else if n == "Method" {
                    if let Some(t) = templates.last_mut() {
                        t.methods.push(HttpMethod {
                            name: String::new(),
                            http_method: None,
                            handler: None,
                        });
                    }
                }
                stack.push(n);
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Text(t)) => {
                let s = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                let s = s.trim().to_string();
                if s.is_empty() {
                    buf.clear();
                    continue;
                }
                let tag = stack.last().map(String::as_str).unwrap_or("");
                let in_props = stack.len() >= 2 && stack[stack.len() - 2] == "Properties";
                if !in_props {
                    buf.clear();
                    continue;
                }
                let in_method = stack.iter().any(|t| t == "Method");
                if let Some(tpl) = templates.last_mut() {
                    if in_method {
                        if let Some(m) = tpl.methods.last_mut() {
                            match tag {
                                "Name" if m.name.is_empty() => m.name = s,
                                "HTTPMethod" => m.http_method = Some(s),
                                "Handler" => m.handler = Some(s),
                                _ => {}
                            }
                        }
                    } else {
                        match tag {
                            "Name" if tpl.name.is_empty() => tpl.name = s,
                            "Template" => tpl.template = Some(s),
                            _ => {}
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    templates.retain(|t| !t.name.is_empty());
    Some(HttpService {
        name,
        root_url,
        templates,
        file: rel.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Записать XML во временный файл с BOM. Тесты читают с диска, а не из
    /// строки: разбор начинается со снятия BOM, и проверять его в обход файла
    /// значило бы проверить не тот путь, которым идут настоящие данные.
    fn во_временный(имя: &str, xml: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("gyrfalcon-integration-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(имя);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&[0xEF, 0xBB, 0xBF]).unwrap();
        f.write_all(xml.as_bytes()).unwrap();
        p
    }

    const ДОКУМЕНТ: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Document uuid="aa11">
    <Properties>
      <Name>АвансовыйОтчет</Name>
      <Posting>Allow</Posting>
      <RegisterRecords>
        <xr:Item xsi:type="xr:MDObjectRef">AccumulationRegister.ПрочиеРасчеты</xr:Item>
        <xr:Item xsi:type="xr:MDObjectRef">AccountingRegister.Хозрасчетный</xr:Item>
        <xr:Item xsi:type="xr:MDObjectRef">InformationRegister.ЦеныНоменклатуры</xr:Item>
      </RegisterRecords>
    </Properties>
  </Document>
</MetaDataObject>"#;

    #[test]
    fn движения_из_объявленного_состава() {
        let p = во_временный("doc.xml", ДОКУМЕНТ);
        let m = parse_declared_movements(&p, "АвансовыйОтчет", "Documents/АвансовыйОтчет.xml");
        assert_eq!(m.len(), 3);
        // Префикс вида снят — форма прежнего инструмента.
        assert_eq!(m[0].register_name, "ПрочиеРасчеты");
        assert_eq!(m[1].register_name, "Хозрасчетный");
        assert!(m.iter().all(|x| x.source == "declared"));
        assert!(m.iter().all(|x| x.document_name == "АвансовыйОтчет"));
    }

    #[test]
    fn документ_без_движений_даёт_пусто_а_не_ошибку() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject><Document uuid="bb22"><Properties>
  <Name>СправкаРасчет</Name><RegisterRecords/>
</Properties></Document></MetaDataObject>"#;
        let p = во_временный("doc-empty.xml", xml);
        let m = parse_declared_movements(&p, "СправкаРасчет", "Documents/СправкаРасчет.xml");
        assert!(
            m.is_empty(),
            "пустой RegisterRecords — факт о документе, а не сбой"
        );
    }

    /// Схема пакета XDTO — то, чего у прежнего инструмента нет вовсе (`types_json` пуст во
    /// всех 434 строках, потому что он не открывает спутник `Ext/Package.bin`).
    const ПАКЕТ: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://v8.1c.ru/8.1/xdto" xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="http://bssys.com/upg/request">
	<property xmlns:d2p1="http://bssys.com/upg/request" name="Request" type="d2p1:Request"/>
	<valueType name="AccBeneficiar" base="xs:string" variety="Atomic" maxLength="34"/>
	<objectType name="Performance">
		<property name="version" type="xs:string"/>
		<property name="items" type="xs:int" lowerBound="0" upperBound="-1"/>
	</objectType>
</package>"#;

    #[test]
    fn типы_пакета_разбираются_из_package_bin() {
        let t = parse_package_types(ПАКЕТ);
        assert_eq!(t.len(), 2, "типа два: valueType и objectType");
        assert_eq!(t[0].kind, "valueType");
        assert_eq!(t[0].name, "AccBeneficiar");
        assert_eq!(t[0].base.as_deref(), Some("xs:string"));
        assert_eq!(t[1].kind, "objectType");
        assert_eq!(t[1].properties.len(), 2);
        assert_eq!(t[1].properties[1].upper_bound.as_deref(), Some("-1"));
    }

    /// Сторожевой тест пойманного дефекта: самозакрытый `<valueType .../>`
    /// закрывает область немедленно. Первая редакция ЭТАЛОННОГО измерителя
    /// этого не учитывала и приписывала свойству верхнего уровня предыдущий
    /// тип — расхождение в 6 свойств на пакете `SBRF_request` (28.08.2026).
    /// Виноват был инструмент сверки, а не парсер; тест держит верную сторону.
    #[test]
    fn свойство_после_самозакрытого_типа_не_приписывается_ему() {
        let xml = r#"<package xmlns="http://v8.1c.ru/8.1/xdto">
	<objectType name="Первый"><property name="а" type="xs:string"/></objectType>
	<valueType name="Второй" base="xs:string"/>
	<property name="верхнего_уровня" type="xs:string"/>
</package>"#;
        let t = parse_package_types(xml);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].properties.len(), 1, "у первого типа своё свойство");
        assert!(
            t[1].properties.is_empty(),
            "самозакрытый тип не имеет свойств, и свойство пакета ему не принадлежит"
        );
    }

    const WEB: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <WebService uuid="cc33">
    <Properties>
      <Name>EquipmentService</Name>
      <Namespace>http://www.1c.ru/EquipmentService</Namespace>
    </Properties>
    <ChildObjects>
      <Operation uuid="dd44">
        <Properties>
          <Name>Connect</Name>
          <XDTOReturningValueType>xs:boolean</XDTOReturningValueType>
          <ProcedureName>Connect</ProcedureName>
        </Properties>
        <ChildObjects>
          <Parameter uuid="ee55">
            <Properties>
              <Name>DeviceID</Name>
              <TransferDirection>In</TransferDirection>
            </Properties>
          </Parameter>
          <Parameter uuid="ff66">
            <Properties><Name>Timeout</Name></Properties>
          </Parameter>
        </ChildObjects>
      </Operation>
    </ChildObjects>
  </WebService>
</MetaDataObject>"#;

    #[test]
    fn web_сервис_с_операцией_и_параметрами() {
        let p = во_временный("web.xml", WEB);
        let s = parse_web_service(&p, "WebServices/EquipmentService.xml").unwrap();
        assert_eq!(s.name, "EquipmentService");
        assert_eq!(s.namespace, "http://www.1c.ru/EquipmentService");
        assert_eq!(s.operations.len(), 1);
        let op = &s.operations[0];
        // Имя операции — её собственное, а не имя первого параметра.
        assert_eq!(op.name, "Connect");
        assert_eq!(op.return_type.as_deref(), Some("xs:boolean"));
        assert_eq!(op.procedure_name.as_deref(), Some("Connect"));
        assert_eq!(op.params, vec!["DeviceID", "Timeout"]);
    }

    const HTTP: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <HTTPService uuid="1122">
    <Properties>
      <Name>ExternalAPI</Name>
      <RootURL>api</RootURL>
    </Properties>
    <ChildObjects>
      <URLTemplate uuid="3344">
        <Properties>
          <Name>ПоказателиМонитораРуководителя</Name>
          <Template>/v1/kpi/</Template>
        </Properties>
        <ChildObjects>
          <Method uuid="5566">
            <Properties>
              <Name>Получить</Name>
              <HTTPMethod>GET</HTTPMethod>
              <Handler>ПоказателиМонитораРуководителяПолучить</Handler>
            </Properties>
          </Method>
        </ChildObjects>
      </URLTemplate>
    </ChildObjects>
  </HTTPService>
</MetaDataObject>"#;

    #[test]
    fn http_сервис_с_шаблоном_и_методом() {
        let p = во_временный("http.xml", HTTP);
        let s = parse_http_service(&p, "HTTPServices/ExternalAPI.xml").unwrap();
        assert_eq!(s.name, "ExternalAPI");
        assert_eq!(s.root_url, "api");
        assert_eq!(s.templates.len(), 1);
        let t = &s.templates[0];
        // Имя шаблона не должно перебиваться именем вложенного метода.
        assert_eq!(t.name, "ПоказателиМонитораРуководителя");
        assert_eq!(t.template.as_deref(), Some("/v1/kpi/"));
        assert_eq!(t.methods.len(), 1);
        assert_eq!(t.methods[0].name, "Получить");
        assert_eq!(t.methods[0].http_method.as_deref(), Some("GET"));
        assert_eq!(
            t.methods[0].handler.as_deref(),
            Some("ПоказателиМонитораРуководителяПолучить")
        );
    }
}
