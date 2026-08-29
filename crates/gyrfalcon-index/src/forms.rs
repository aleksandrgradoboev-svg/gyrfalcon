//! Элементы управляемых форм — таблица `form_elements`.
//!
//! Вторая по величине таблица описи (223 291 строка у прежнего инструмента на БП).
//! Три вида в одной таблице, различаются столбцом `kind`:
//!
//! | Вид | Где в XML | Строк у прежнего инструмента |
//! |---|---|---|
//! | `attribute` | `<Attributes>/<Attribute name=…>` | 101 888 |
//! | `handler` | `<Events>/<Event name=…>` | 76 478 |
//! | `command` | `<Commands>/<Command name=…>` | 44 925 |
//!
//! # Обработчики бывают трёх областей
//!
//! Столбец `scope` у прежнего инструмента: `element` 51 088, `form` 16 881, `ext_info` 8 509.
//! Разница не в виде события, а в том, ЧЕЙ это обработчик:
//!
//! ```text
//! <Form>
//!   <Events>                        ← scope=form: событие самой формы
//!     <Event name="OnOpen">ПриОткрытии</Event>
//!   <ChildItems>
//!     <InputField name="Дата">
//!       <Events>                    ← scope=element: событие элемента
//!         <Event name="OnChange">ДатаПриИзменении</Event>
//!   <AutoCommandBar>… (ext_info у прежнего инструмента — служебные разделы формы)
//! ```
//!
//! У `element` заполняются `element_name`, `element_type` (`InputField`,
//! `LabelField`, …) и `data_path`; у `form` и `ext_info` они пусты.
//! Проверено по прежнему инструменту: непустой `element_name` ровно у 51 088 строк —
//! столько же, сколько записей со `scope='element'`.
//!
//! # Откуда взяты поля
//!
//! Столбцы — с индекса прежнего инструмента (`sqlite_master`), значения сверены с живой
//! выгрузкой БП 28.08.2026. Форма тегов прочитана в файлах.

use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader;
use std::path::Path;

/// Строка таблицы `form_elements`.
#[derive(Debug, Clone, Default)]
pub struct FormElement {
    pub object_name: String,
    /// Категория каталога выгрузки: `Catalogs`, `Documents`, `CommonForms`.
    pub category: String,
    pub form_name: String,
    /// `attribute` | `handler` | `command`.
    pub kind: &'static str,
    /// Для обработчиков: `form` | `element` | `ext_info`. Иначе пусто.
    pub scope: String,
    pub element_name: String,
    /// У реквизита — тип (`cfg:CatalogRef.X`), у обработчика элемента —
    /// вид элемента (`InputField`).
    pub element_type: String,
    pub event: String,
    pub handler: String,
    pub data_path: String,
    pub main_table: String,
    pub attribute_is_main: bool,
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

fn attr(e: &quick_xml::events::BytesStart, want: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (local_name(a.key.as_ref()) == want)
            .then(|| String::from_utf8_lossy(a.value.as_ref()).into_owned())
    })
}

/// Виды узлов, которые НЕ являются элементами формы, хотя лежат в `ChildItems`.
///
/// Служебные разделы формы: автоматическая командная панель, контекстное меню,
/// расширенная подсказка. У прежнего инструмента обработчики из них помечены `ext_info`.
const СЛУЖЕБНЫЕ: &[&str] = &[
    "AutoCommandBar",
    "ContextMenu",
    "ExtendedTooltip",
    "SearchStringAddition",
    "ViewStatusAddition",
    "SearchControlAddition",
];

/// События формы, которые прежний инструмент помечает `ext_info`, а не `form`.
///
/// Метка зависит **от имени события, а не от места в XML**: все события формы
/// лежат в одной секции `<Events>` под корнем. Список закрытый и снят с фактов
/// (28.08.2026): у прежнего инструмента `scope='ext_info'` встречается ровно у семи видов
/// событий, `scope='form'` — у тридцати, и **множества имён не пересекаются
/// ни в одной строке** из 25 390. Общее у семёрки — жизненный цикл записи
/// и закрытия формы.
///
/// Выведено сверкой: сначала все они писались как `form`, и расхождение
/// в 8 380 строк объяснялось только этим — сам факт обработчика совпадал,
/// различалась метка.
const СОБЫТИЯ_EXT_INFO: &[&str] = &[
    "BeforeWrite",
    "BeforeWriteAtServer",
    "AfterWrite",
    "AfterWriteAtServer",
    "OnReadAtServer",
    "BeforeClose",
    "OnClose",
];

/// Разобрать `Form.xml` целиком: реквизиты, обработчики, команды.
///
/// Один проход по документу: три вида лежат в разных секциях, но читать файл
/// трижды ради этого незачем — их 7 874 штуки.
pub fn parse_form(
    path: &Path,
    object_name: &str,
    category: &str,
    form_name: &str,
    rel: &str,
) -> Vec<FormElement> {
    let Some(text) = read_text(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut reader = reader_for(&text);
    let mut buf = Vec::new();

    // Стек имён тегов — по нему определяется секция и вложенность.
    let mut stack: Vec<String> = Vec::new();
    // Текущий элемент формы (имя, вид) — ближайший предок-элемент.
    let mut элемент: Vec<(String, String)> = Vec::new();
    // Накопители для текущего разбираемого узла.
    let mut текущий_реквизит: Option<FormElement> = None;
    let mut текущая_команда: Option<FormElement> = None;
    let mut текущее_событие: Option<String> = None;
    // Путь данных хранится ПО ЭЛЕМЕНТУ, а не одной переменной на весь разбор.
    //
    // Элементы вложены: у `<Table>` внутри лежит `<AutoCommandBar>` со своими
    // кнопками, и общая переменная затиралась при выходе из вложенного —
    // путь самой таблицы терялся. Стек здесь ровно тот же, что у `элемент`,
    // и растёт-убывает вместе с ним.
    let mut пути: Vec<Option<String>> = Vec::new();
    // Глубина `Type` внутри реквизита: тип берётся только у самого реквизита,
    // а не у вложенных квалификаторов.
    let mut в_типе = false;
    // Глубина стека, на которой открылся внешний `<Type>` реквизита.
    let mut глубина_типа = usize::MAX;

    let образец = FormElement {
        object_name: object_name.to_string(),
        category: category.to_string(),
        form_name: form_name.to_string(),
        file: rel.to_string(),
        ..Default::default()
    };

    loop {
        match reader.read_event_into(&mut buf) {
            // Самозакрытый тег (`<ExtendedTooltip name="…"/>`) не порождает
            // ни текста, ни парного `End`. Открывать по нему область нельзя —
            // стек разъедется на первом же элементе. В выгрузке форм таких
            // тегов много: 448 против 3 271 открывающих на одной форме.
            Ok(XmlEvent::Empty(_)) => {}

            Ok(XmlEvent::Start(e)) => {
                let имя = local_name(e.name().as_ref());

                match имя.as_str() {
                    "Attribute" if stack.iter().any(|t| t == "Attributes") => {
                        let mut r = образец.clone();
                        r.kind = "attribute";
                        r.element_name = attr(&e, "name").unwrap_or_default();
                        текущий_реквизит = Some(r);
                    }
                    "Command" if stack.iter().any(|t| t == "Commands") => {
                        let mut c = образец.clone();
                        c.kind = "command";
                        c.element_name = attr(&e, "name").unwrap_or_default();
                        текущая_команда = Some(c);
                    }
                    "Event" => текущее_событие = attr(&e, "name"),
                    // `<Type>` берётся только НЕПОСРЕДСТВЕННО из `<Attribute>`.
                    // У реквизита-таблицы значений есть ещё `<Columns>/<Column>`
                    // со своим `<Type>`, и без этой проверки тип колонки
                    // приклеивается к типу реквизита: `…ЗаявкаОтпуск, xs:decimal`.
                    // Поймано сверкой — 18 893 реквизита на БП.
                    "Type"
                        if текущий_реквизит.is_some()
                            && stack.last().map(String::as_str) == Some("Attribute") =>
                    {
                        в_типе = true;
                        глубина_типа = stack.len() + 1;
                    }
                    // Элемент формы: любой именованный узел внутри ChildItems,
                    // кроме служебных разделов.
                    _ if stack.iter().any(|t| t == "ChildItems") => {
                        if let Some(n) = attr(&e, "name") {
                            if !СЛУЖЕБНЫЕ.contains(&имя.as_str()) {
                                элемент.push((n, имя.clone()));
                            } else {
                                элемент.push((String::new(), имя.clone()));
                            }
                            пути.push(None);
                        }
                    }
                    _ => {}
                }
                stack.push(имя);
            }

            Ok(XmlEvent::Text(t)) => {
                let s = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                let s = s.trim().to_string();
                if s.is_empty() {
                    buf.clear();
                    continue;
                }
                let тег = stack.last().map(String::as_str).unwrap_or("");
                match тег {
                    // Тип реквизита. У составного типа внутри `<Type>` лежит
                    // НЕСКОЛЬКО `<v8:Type>`, и прежний инструмент склеивает их через `, `:
                    // `cfg:CatalogRef.Роли, cfg:CatalogRef.Пользователи`.
                    // Брать только первый — терять половину типа; на БП это
                    // 448 реквизитов, найдено сверкой.
                    "Type" if в_типе => {
                        if let Some(r) = текущий_реквизит.as_mut() {
                            if r.element_type.is_empty() {
                                r.element_type = s;
                            } else {
                                r.element_type.push_str(", ");
                                r.element_type.push_str(&s);
                            }
                        }
                    }
                    "MainAttribute" => {
                        if let Some(r) = текущий_реквизит.as_mut() {
                            r.attribute_is_main = s == "true";
                        }
                    }
                    "Action" => {
                        if let Some(c) = текущая_команда.as_mut() {
                            c.handler = s;
                        }
                    }
                    // `DataPath` берётся только НЕПОСРЕДСТВЕННО у элемента.
                    // Вложенные `<xr:DataPath>` встречаются в двух местах и
                    // означают не данные элемента, а связи:
                    // `<ChoiceParameterLinks>/<xr:Link>` — параметр выбора,
                    // `<TypeLink>` — связь по типу. Оба затирали настоящий путь.
                    // Поймано сверкой: 14 962 обработчика с пустым `data_path`.
                    "DataPath"
                        if !stack
                            .iter()
                            .any(|t| t == "ChoiceParameterLinks" || t == "TypeLink") =>
                    {
                        if let Some(слот) = пути.last_mut() {
                            *слот = Some(s);
                        }
                    }
                    // Текст события — имя обработчика.
                    "Event" => {
                        if let Some(ev) = текущее_событие.take() {
                            let mut h = образец.clone();
                            h.kind = "handler";
                            h.event = ev;
                            h.handler = s;
                            // Область определяется ближайшим предком-элементом.
                            match элемент.last() {
                                Some((n, вид)) if !n.is_empty() => {
                                    h.scope = "element".into();
                                    h.element_name = n.clone();
                                    h.element_type = вид.clone();
                                    h.data_path =
                                        пути.last().cloned().flatten().unwrap_or_default();
                                }
                                // Обработчик служебного раздела формы
                                // (командная панель, контекстное меню).
                                Some((_, _)) => h.scope = "ext_info".into(),
                                // Событие самой формы: метка зависит от ИМЕНИ
                                // события, а не от места в XML — все они лежат
                                // в одной секции под корнем.
                                None => {
                                    h.scope = if СОБЫТИЯ_EXT_INFO.contains(&h.event.as_str())
                                    {
                                        "ext_info".into()
                                    } else {
                                        "form".into()
                                    }
                                }
                            }
                            out.push(h);
                        }
                    }
                    _ => {}
                }
            }

            Ok(XmlEvent::End(e)) => {
                let имя = local_name(e.name().as_ref());
                match имя.as_str() {
                    "Attribute" => {
                        if let Some(r) = текущий_реквизит.take() {
                            if !r.element_name.is_empty() {
                                out.push(r);
                            }
                        }
                    }
                    "Command" => {
                        if let Some(c) = текущая_команда.take() {
                            if !c.element_name.is_empty() {
                                out.push(c);
                            }
                        }
                    }
                    // Закрытие внешнего `<Type>` реквизита. Вложенные
                    // `</v8:Type>` локально называются так же, и снимать флаг
                    // по ним нельзя: у составного типа их несколько подряд,
                    // и второй с третьим потерялись бы. Различаем по глубине —
                    // внешний закрывается там же, где открывался.
                    "Type" if stack.len() == глубина_типа => {
                        в_типе = false;
                        глубина_типа = usize::MAX;
                    }
                    _ => {
                        // Выход из элемента формы — снять его со стека.
                        if stack.iter().filter(|t| *t == "ChildItems").count() > 0
                            && !элемент.is_empty()
                            && элемент.last().map(|(_, в)| в.as_str()) == Some(имя.as_str())
                        {
                            элемент.pop();
                            пути.pop();
                        }
                    }
                }
                stack.pop();
            }

            Ok(XmlEvent::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn во_временный(имя: &str, xml: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("gyrfalcon-forms-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(имя);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&[0xEF, 0xBB, 0xBF]).unwrap();
        f.write_all(xml.as_bytes()).unwrap();
        p
    }

    fn разобрать(имя: &str, xml: &str) -> Vec<FormElement> {
        let p = во_временный(имя, xml);
        parse_form(&p, "Тест", "Documents", "ФормаДокумента", "x")
    }

    const ФОРМА: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable">
  <Events>
    <Event name="OnOpen">ПриОткрытии</Event>
    <Event name="AfterWriteAtServer">ПослеЗаписиНаСервере</Event>
  </Events>
  <ChildItems>
    <InputField name="Дата">
      <DataPath>Объект.Дата</DataPath>
      <ContextMenu name="ДатаКонтекстноеМеню"/>
      <Events>
        <Event name="OnChange">ДатаПриИзменении</Event>
      </Events>
    </InputField>
  </ChildItems>
  <Attributes>
    <Attribute name="Объект">
      <Type>
        <v8:Type>cfg:DocumentObject.Тест</v8:Type>
      </Type>
      <MainAttribute>true</MainAttribute>
    </Attribute>
  </Attributes>
  <Commands>
    <Command name="Провести">
      <Action>ПровестиВыполнить</Action>
    </Command>
  </Commands>
</Form>"#;

    #[test]
    fn три_вида_разбираются() {
        let r = разобрать("базовая.xml", ФОРМА);
        let реквизиты: Vec<_> = r.iter().filter(|e| e.kind == "attribute").collect();
        let команды: Vec<_> = r.iter().filter(|e| e.kind == "command").collect();
        let обработчики: Vec<_> = r.iter().filter(|e| e.kind == "handler").collect();

        assert_eq!(реквизиты.len(), 1);
        assert_eq!(реквизиты[0].element_name, "Объект");
        assert_eq!(реквизиты[0].element_type, "cfg:DocumentObject.Тест");
        assert!(реквизиты[0].attribute_is_main);

        assert_eq!(команды.len(), 1);
        assert_eq!(команды[0].handler, "ПровестиВыполнить");

        assert_eq!(обработчики.len(), 3);
    }

    /// Метка `ext_info` зависит от ИМЕНИ события, а не от места в XML:
    /// все события формы лежат в одной секции под корнем. Проверено по
    /// прежнему инструменту — множества имён `form` и `ext_info` не пересекаются.
    /// Расхождение в 8 380 строк объяснялось только этим.
    #[test]
    fn область_события_формы_по_имени_события() {
        let r = разобрать("области.xml", ФОРМА);
        let по_событию = |ev: &str| {
            r.iter()
                .find(|e| e.kind == "handler" && e.event == ev)
                .map(|e| e.scope.clone())
                .unwrap_or_default()
        };
        assert_eq!(по_событию("OnOpen"), "form");
        assert_eq!(
            по_событию("AfterWriteAtServer"),
            "ext_info",
            "события жизненного цикла записи прежний инструмент помечает ext_info"
        );
        assert_eq!(по_событию("OnChange"), "element");
    }

    /// Сторожевой тест: у составного типа внутри `<Type>` несколько
    /// `<v8:Type>`, и прежний инструмент склеивает их через `, `. Брать только первый —
    /// терять половину типа (448 реквизитов на БП).
    #[test]
    fn составной_тип_склеивается_целиком() {
        let xml = r#"<Form xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <Attributes>
    <Attribute name="Исполнитель">
      <Type>
        <v8:Type>cfg:CatalogRef.Роли</v8:Type>
        <v8:Type>cfg:CatalogRef.Пользователи</v8:Type>
      </Type>
    </Attribute>
  </Attributes>
</Form>"#;
        let r = разобрать("составной.xml", xml);
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0].element_type,
            "cfg:CatalogRef.Роли, cfg:CatalogRef.Пользователи"
        );
    }

    /// Сторожевой тест: у реквизита-таблицы значений есть `<Columns>/<Column>`
    /// со своим `<Type>`. Без проверки на непосредственное вложение тип
    /// колонки приклеивался к типу реквизита — 18 893 реквизита на БП.
    #[test]
    fn тип_колонки_не_приклеивается_к_типу_реквизита() {
        let xml = r#"<Form xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <Attributes>
    <Attribute name="Задание">
      <Type>
        <v8:Type>cfg:BusinessProcessObject.Заявка</v8:Type>
      </Type>
      <Columns>
        <AdditionalColumns table="Задание.Отпуска">
          <Column name="Статус">
            <Type>
              <v8:Type>xs:decimal</v8:Type>
            </Type>
          </Column>
        </AdditionalColumns>
      </Columns>
    </Attribute>
  </Attributes>
</Form>"#;
        let r = разобрать("колонки.xml", xml);
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0].element_type, "cfg:BusinessProcessObject.Заявка",
            "тип колонки не должен попадать в тип реквизита"
        );
    }

    /// Сторожевой тест: вложенные `<xr:DataPath>` в `<TypeLink>` и
    /// `<ChoiceParameterLinks>` — это связи, а не данные элемента.
    /// Они затирали настоящий путь у 14 962 обработчиков.
    #[test]
    fn вложенный_datapath_связей_не_затирает_путь_элемента() {
        let xml = r#"<Form xmlns:xr="http://v8.1c.ru/8.3/xcf/readable">
  <ChildItems>
    <InputField name="Субконто1">
      <DataPath>Билеты.Субконто1</DataPath>
      <TypeLink>
        <xr:DataPath>Items.Билеты.CurrentData.СчетЗатрат</xr:DataPath>
      </TypeLink>
      <ChoiceParameterLinks>
        <xr:Link>
          <xr:DataPath>Задание.ТипЗаявки</xr:DataPath>
        </xr:Link>
      </ChoiceParameterLinks>
      <Events>
        <Event name="OnChange">Субконто1ПриИзменении</Event>
      </Events>
    </InputField>
  </ChildItems>
</Form>"#;
        let r = разобрать("связи.xml", xml);
        let h = r
            .iter()
            .find(|e| e.kind == "handler")
            .expect("нет обработчика");
        assert_eq!(h.data_path, "Билеты.Субконто1");
    }

    /// Сторожевой тест: путь данных хранится ПО ЭЛЕМЕНТУ. У `<Table>` внутри
    /// лежит `<AutoCommandBar>` со своими кнопками, и одна общая переменная
    /// затиралась при выходе из вложенного — путь таблицы терялся.
    #[test]
    fn путь_таблицы_переживает_вложенную_панель() {
        let xml = r#"<Form>
  <ChildItems>
    <Table name="Отпуска">
      <DataPath>Задание.Отпуска</DataPath>
      <AutoCommandBar name="ОтпускаКоманднаяПанель">
        <ChildItems>
          <Button name="Добавить"/>
        </ChildItems>
      </AutoCommandBar>
      <Events>
        <Event name="OnEditEnd">ОтпускаПриОкончанииРедактирования</Event>
      </Events>
    </Table>
  </ChildItems>
</Form>"#;
        let r = разобрать("таблица.xml", xml);
        let h = r
            .iter()
            .find(|e| e.kind == "handler")
            .expect("нет обработчика");
        assert_eq!(h.element_name, "Отпуска");
        assert_eq!(h.data_path, "Задание.Отпуска");
    }

    /// Самозакрытый тег не открывает область: парного `End` у него нет,
    /// и стек бы разъехался. В выгрузке форм таких тегов 448 на 3 271
    /// открывающих. Целевые теги самозакрытыми не бывают — проверено
    /// на 6 008 тегах в 200 формах документов, ноль случаев.
    #[test]
    fn самозакрытый_тег_не_ломает_вложенность() {
        let xml = r#"<Form>
  <ChildItems>
    <InputField name="Поле">
      <DataPath>Объект.Поле</DataPath>
      <ContextMenu name="ПолеКонтекстноеМеню"/>
      <ExtendedTooltip name="ПолеПодсказка"/>
      <Events>
        <Event name="OnChange">ПолеПриИзменении</Event>
      </Events>
    </InputField>
  </ChildItems>
</Form>"#;
        let r = разобрать("самозакрытый.xml", xml);
        let h = r
            .iter()
            .find(|e| e.kind == "handler")
            .expect("нет обработчика");
        assert_eq!(h.scope, "element");
        assert_eq!(
            h.element_name, "Поле",
            "стек разъехался на самозакрытом теге"
        );
        assert_eq!(h.data_path, "Объект.Поле");
    }

    #[test]
    fn форма_без_секций_даёт_пусто_а_не_ошибку() {
        let r = разобрать("пустая.xml", r#"<Form><ChildItems/></Form>"#);
        assert!(r.is_empty());
    }
}
