//! Полный разбор модуля: методы, области, вызовы с привязкой к владельцу.
//!
//! В отличие от [`crate::bsl`], который отдаёт плоские списки для замеров,
//! здесь собирается всё, что нужно индексу за **один** проход по дереву:
//! второй обход тех же 6,2 ГБ стоит столько же, сколько первый.
//!
//! # Как в AST выглядит вызов
//!
//! Проверено дампом дерева на живом коде (28.08.2026), а не по памяти:
//!
//! ```text
//! call_statement                       ОбщегоНазначения.Метод(А);
//!   call_expression                    ОбщегоНазначения.Метод(А)
//!     access                           ОбщегоНазначения      ← квалификатор
//!     .
//!     method_call                      Метод(А)
//!       identifier                     Метод                 ← имя
//! ```
//!
//! Простой вызов — `method_call` **без** родителя `call_expression`.
//! Отсюда правило сборки полного имени: текст `access` + точка + имя метода.
//!
//! Прежняя реализация [`crate::bsl::collect_calls`] брала только имя и теряла
//! квалификатор: `ОбщегоНазначения.ЗначениеРеквизита` приходило как
//! `ЗначениеРеквизита`. Разрешить такое имя правильно нельзя — оно не отличимо
//! от локального вызова.

use crate::{ParseError, Result};
use tree_sitter::{Node, Parser};

/// Метод модуля со всем, что о нём знает индекс.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub name: String,
    /// Как написано в исходнике: `Процедура` / `Функция`.
    /// Регистр НЕ нормализуется — прежний инструмент хранит его как есть, и при сверке
    /// поимённо нормализация дала бы расхождение на ровном месте.
    pub kind: String,
    pub is_export: bool,
    /// Параметры одной строкой, как в объявлении.
    pub params: String,
    pub line_start: u32,
    pub line_end: u32,
}

impl Method {
    /// Строк в теле метода — столбец `methods.loc` у прежнего инструмента.
    pub fn loc(&self) -> u32 {
        self.line_end.saturating_sub(self.line_start) + 1
    }
}

/// Область `#Область Имя` … `#КонецОбласти`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub name: String,
    pub line: u32,
    pub end_line: Option<u32>,
}

/// Вызов в точке употребления — ещё не ребро графа.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// Полное имя с квалификатором: `ОбщегоНазначения.ЗначениеРеквизита`.
    pub name: String,
    pub line: u32,
    /// Индекс метода-владельца в [`ParsedModule::methods`].
    /// `None` — вызов вне метода (в теле модуля).
    pub caller: Option<usize>,
    /// Сколько аргументов передано в точке вызова.
    ///
    /// Нужно, чтобы сверять вызов с сигнатурой из индекса: «метод есть, но
    /// зовут с четырьмя аргументами при трёх объявленных» — ошибка, которую
    /// не видят ни AST, ни проверка существования имени.
    pub arg_count: usize,
}

/// Аннотация перехвата расширения: `&ИзменениеИКонтроль("Имя")`.
///
/// Грамматика отдаёт всю конструкцию узлом `preprocessor` (НЕ `annotation`,
/// как можно решить по `node-types.json`): внутри лежат `annotation` с именем
/// и `string` с целевым методом. Директивы компиляции (`&НаСервере`,
/// `&НаКлиенте`) имеют ту же форму, но БЕЗ строкового аргумента — этим и
/// отличаются, а не списком известных имён: список устареет с новой платформой.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    /// Вид перехвата: `ИзменениеИКонтроль`, `После`, `Перед`, `Вместо`.
    pub annotation: String,
    /// Перехватываемый метод основной конфигурации — аргумент аннотации.
    pub target_method: String,
    /// Строка самой аннотации (адрес, который хранит прежний инструмент).
    pub line: u32,
    /// Индекс метода-перехватчика в [`ParsedModule::methods`].
    /// `None` — аннотация без следующего за ней объявления.
    pub method: Option<usize>,
    /// Имя метода-перехватчика: `Расш1_ПриЗаписи`.
    pub method_name: String,
}

/// Разобранный модуль целиком.
#[derive(Debug, Clone, Default)]
pub struct ParsedModule {
    pub methods: Vec<Method>,
    pub regions: Vec<Region>,
    pub calls: Vec<Call>,
    /// Перехваты расширений — пусто у модулей основной конфигурации.
    pub overrides: Vec<Override>,
    /// Комментарий-шапка модуля: строки `//` до первого объявления.
    pub header_comment: Option<String>,
    /// Упоминания объектов метаданных — сырьё `metadata_code_usages`.
    /// Собираются тем же проходом: второй обход тех же 6,2 ГБ стоит
    /// столько же, сколько первый.
    pub usages: Vec<crate::usages::MetadataUsage>,
    /// В дереве есть узлы-ошибки.
    pub has_errors: bool,
    /// Имена, объявленные В САМОМ модуле: переменные, параметры, счётчики циклов.
    ///
    /// Нужны, чтобы отличить `ОбщегоНазначения.Метод()` (обращение к модулю
    /// конфигурации, проверяемо) от `Контрагенты.Количество()` (обращение к
    /// локальному массиву, который лишь совпал именем со справочником).
    /// Без этого списка проверка имён даёт ложные тревоги на совершенно
    /// верном коде — замер 05.09.2026 на Документообороте: 1,14%, и ВСЕ
    /// до единой именно такие.
    pub locals: Vec<String>,
}

/// Разобрать модуль за один проход.
pub fn parse(source: &str) -> Result<ParsedModule> {
    let mut parser = Parser::new();
    parser
        .set_language(&crate::bsl::language())
        .map_err(|e| ParseError::Language(e.to_string()))?;
    let tree = parser.parse(source, None).ok_or(ParseError::NoTree)?;
    Ok(parse_tree(&tree, source))
}

/// Разобрать уже готовое дерево — для конвейера, где парсер живёт в потоке.
pub fn parse_tree(tree: &tree_sitter::Tree, source: &str) -> ParsedModule {
    let bytes = source.as_bytes();
    let mut out = ParsedModule {
        has_errors: tree.root_node().has_error(),
        header_comment: header_comment(source),
        ..Default::default()
    };

    // Обход в глубину с явным стеком: рекурсия на чужом дереве
    // неизвестной глубины — способ получить переполнение стека на одном
    // сгенерированном модуле из восемнадцати тысяч.
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        match node.kind() {
            "procedure_definition" | "function_definition" => {
                if let Some(m) = method_of(node, bytes) {
                    out.methods.push(m);
                }
            }
            "preprocessor" => {
                collect_region(node, bytes, &mut out.regions);
                if let Some(o) = override_of(node, bytes) {
                    out.overrides.push(o);
                }
            }
            // Локальные имена: левая часть присваивания, объявления Перем,
            // счётчик Для Каждого. Параметры методов добавляются в method_of.
            "assignment_statement" | "var_statement" | "for_each_statement" => {
                collect_local(node, bytes, &mut out.locals);
            }
            "method_call" => {
                if let Some(c) = call_of(node, bytes) {
                    out.calls.push(c);
                }
            }
            // Обращение к менеджеру. Узлов два, потому что форма записи меняет
            // форму дерева: с вызовом метода имя лежит в `access`, без вызова —
            // в родителе `property_access` (см. `usages::упоминание_в_access`).
            //
            // `property_access` берётся ТОЛЬКО когда его вложенный `access`
            // односоставный: иначе то же упоминание сосчиталось бы дважды.
            "access" => {
                if let Some(u) = crate::usages::упоминание_в_access(node, bytes) {
                    out.usages.push(u);
                }
            }
            "property_access" => {
                // Берём ТОЛЬКО голову цепочки: `Перечисления.Вид.Значение` —
                // это `property_access` над `property_access`, и разбирать
                // надо внутренний (две части), иначе имя объекта окажется
                // третьим сегментом. Внешний узел пропускаем, когда его
                // первый потомок сам содержит точку.
                let вложенный_составной = node
                    .named_child(0)
                    .and_then(|c| c.utf8_text(bytes).ok())
                    .is_some_and(|t| t.contains('.'));
                if !вложенный_составной {
                    if let Some(u) = crate::usages::упоминание_в_access(node, bytes) {
                        out.usages.push(u);
                    }
                }
            }
            // Содержимое строкового литерала без кавычек. У многострочного
            // литерала (текст запроса) на каждую строку приходит СВОЙ узел
            // со своим номером — номера строк получаются даром.
            "string_content" => {
                if let Ok(t) = node.utf8_text(bytes) {
                    let line = node.start_position().row as u32 + 1;
                    crate::usages::упоминания_в_литерале(
                        t,
                        line,
                        &mut out.usages,
                    );
                }
            }
            _ => {}
        }
        stack.extend(node.children(&mut cursor));
    }

    out.methods.sort_by_key(|m| m.line_start);
    // Перехватчик — ПЕРВЫЙ метод, объявленный ниже аннотации. Связываем
    // после сортировки: обход идёт стеком, и в порядке появления методы
    // приходить не обязаны. Между аннотацией и объявлением законно стоят
    // директивы компиляции (`&НаСервере`) и комментарии — поэтому «первый
    // ниже», а не «ровно на следующей строке».
    out.overrides.sort_by_key(|o| o.line);
    for o in &mut out.overrides {
        if let Some(i) = out.methods.iter().position(|m| m.line_start > o.line) {
            o.method = Some(i);
            o.method_name = out.methods[i].name.clone();
        }
    }
    out.regions.sort_by_key(|r| r.line);
    out.calls.sort_by_key(|c| c.line);
    out.usages.sort_by(|a, b| {
        a.line
            .cmp(&b.line)
            .then_with(|| a.object_ref_key.cmp(&b.object_ref_key))
    });

    // Параметры методов — тоже локальные имена: `Процедура П(Контрагенты)`
    // делает `Контрагенты.Количество()` обращением к параметру, а не к справочнику.
    for m in &out.methods {
        for p in m
            .params
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(',')
        {
            let имя = p
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches("Знач ")
                .trim();
            if !имя.is_empty() {
                out.locals.push(имя.to_string());
            }
        }
    }
    out.locals.sort();
    out.locals.dedup();

    // Владелец вызова — метод, в чьи границы строк вызов попадает.
    // Считается после сортировки обоих списков.
    for call in &mut out.calls {
        call.caller = out
            .methods
            .iter()
            .position(|m| call.line >= m.line_start && call.line <= m.line_end);
    }

    out
}

fn method_of(node: Node, bytes: &[u8]) -> Option<Method> {
    let name = node.child_by_field_name("name")?.utf8_text(bytes).ok()?;
    let kind = node
        .child(0)
        .and_then(|k| k.utf8_text(bytes).ok())
        .unwrap_or("Процедура")
        .to_string();
    let params = node
        .child_by_field_name("parameters")
        .and_then(|p| p.utf8_text(bytes).ok())
        .unwrap_or("()")
        .trim()
        .to_string();

    Some(Method {
        name: name.to_string(),
        kind,
        is_export: node.child_by_field_name("export").is_some(),
        params,
        line_start: node.start_position().row as u32 + 1,
        line_end: node.end_position().row as u32 + 1,
    })
}

/// Собрать полное имя вызова, включая квалификатор.
fn call_of(node: Node, bytes: &[u8]) -> Option<Call> {
    let name = node.child_by_field_name("name")?.utf8_text(bytes).ok()?;
    let line = node.start_position().row as u32 + 1;

    // Родитель `call_expression` означает квалифицированный вызов:
    // его первый именованный потомок — `access` с текстом квалификатора.
    let full = node
        .parent()
        .filter(|p| p.kind() == "call_expression")
        .and_then(|p| p.child(0))
        .filter(|a| a.kind() == "access")
        .and_then(|a| a.utf8_text(bytes).ok())
        .map_or_else(
            || name.to_string(),
            |qual| format!("{}.{}", qual.trim(), name),
        );

    Some(Call {
        name: full,
        line,
        caller: None,
        arg_count: count_args(node, bytes),
    })
}

/// Собрать имя, объявляемое этим оператором.
///
/// Берётся ПЕРВЫЙ идентификатор оператора: у присваивания это левая часть,
/// у `Перем` — имя переменной, у `Для Каждого` — счётчик. Точность здесь
/// намеренно грубая в одну сторону: лишнее имя в списке локальных ведёт к
/// пропуску проверки (молчим там, где могли бы сказать), а недостающее — к
/// ложной тревоге на верном коде. Первое дешевле второго.
fn collect_local(node: Node, bytes: &[u8], out: &mut Vec<String>) {
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        match ch.kind() {
            "identifier" | "access" | "property_access" => {
                if let Ok(t) = ch.utf8_text(bytes) {
                    // У `А.Б = 1` объявляется не `А.Б`, а обращение к полю уже
                    // существующего `А` — берём корень до первой точки.
                    let корень = t.split(['.', '[']).next().unwrap_or(t).trim();
                    if !корень.is_empty() {
                        out.push(корень.to_string());
                    }
                }
                return;
            }
            _ => {}
        }
    }
}

/// Сколько аргументов передано в точке вызова.
///
/// Считается по запятым верхнего уровня, а не по узлам дерева: аргументом
/// бывает выражение, вложенный вызов, тернарник — узлы разные, запятая одна
/// на всех. Вложенные скобки и строковые литералы пропускаются, иначе
/// `Ф(Г(А, Б), "х,у")` дал бы четыре аргумента вместо двух.
fn count_args(node: Node, bytes: &[u8]) -> usize {
    let Ok(text) = node.utf8_text(bytes) else {
        return 0;
    };
    let Some(open) = text.find('(') else { return 0 };
    let mut depth = 0i32;
    let mut in_str = false;
    let mut commas = 0usize;
    let mut есть_содержимое = false;
    for ch in text[open..].chars() {
        match ch {
            // Кавычка и всё внутри строки — это содержимое аргумента.
            // Без этой отметки `Ф("привет")` считался нулём аргументов:
            // единственный символ внутри скобок оказывался «строковым» и
            // не попадал в признак непустоты (поймано живым прогоном 05.09.2026).
            '"' => {
                in_str = !in_str;
                if depth == 1 {
                    есть_содержимое = true;
                }
            }
            _ if in_str => {}
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            ',' if depth == 1 => commas += 1,
            c if depth == 1 && !c.is_whitespace() => есть_содержимое = true,
            _ => {}
        }
    }
    // `Ф()` — ноль; `Ф(, Б)` — два: пропуск позиционного аргумента в BSL
    // законен и означает «значение по умолчанию».
    if commas == 0 && !есть_содержимое {
        0
    } else {
        commas + 1
    }
}

/// Собрать область из узла препроцессора.
///
/// Проверено дампом дерева 28.08.2026: узел называется `preprocessor`, и он
/// **объемлющий** — содержимое области лежит внутри него, а `#Область`
/// и `#КонецОбласти` это первый и последний потомки:
///
/// ```text
/// preprocessor
///   PREPROC_REGION_KEYWORD      #Область
///   identifier                  Обработчики   ← имя
///   procedure_definition        …содержимое…
///   PREPROC_ENDREGION_KEYWORD   #КонецОбласти
/// ```
///
/// Значит границы берутся прямо из узла, а парность считать стеком не нужно:
/// её уже посчитала грамматика. Первая редакция этой функции искала пару
/// маркеров вручную и не находила ничего — узел назывался иначе, чем я
/// предположил, и тест это поймал.
/// Перехват расширения из узла `preprocessor`, если это он.
///
/// Отличаем перехват от директивы компиляции ПО ФОРМЕ, а не по имени:
/// у перехвата есть строковый аргумент — имя метода основной конфигурации,
/// у `&НаСервере` его нет. Список видов перехвата при этом не зашит: что
/// платформа добавит завтра, разберётся сегодняшним кодом.
fn override_of(node: Node, bytes: &[u8]) -> Option<Override> {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();

    let annotation = children.iter().find(|n| n.kind() == "annotation")?;
    let name = annotation
        .utf8_text(bytes)
        .ok()?
        .trim_start_matches('&')
        .trim();
    if name.is_empty() {
        return None;
    }

    // Аргумент: строковый литерал в той же конструкции. Кавычки снимаем —
    // прежний инструмент хранит голое имя метода.
    let target = children
        .iter()
        .find(|n| n.kind() == "string")
        .and_then(|n| n.utf8_text(bytes).ok())?
        .trim()
        .trim_matches('"')
        .trim()
        .to_string();
    if target.is_empty() {
        return None;
    }

    Some(Override {
        annotation: name.to_string(),
        target_method: target,
        line: node.start_position().row as u32 + 1,
        method: None,
        method_name: String::new(),
    })
}

fn collect_region(node: Node, bytes: &[u8], done: &mut Vec<Region>) {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();

    // Это область, а не `#Если`: первый потомок — ключевое слово области.
    if children.first().map(Node::kind) != Some("PREPROC_REGION_KEYWORD") {
        return;
    }

    let name = children
        .get(1)
        .filter(|n| n.kind() == "identifier")
        .and_then(|n| n.utf8_text(bytes).ok())
        .unwrap_or("БезИмени")
        .to_string();

    // Конец есть, только если грамматика нашла закрывающий маркер.
    // Незакрытая область — не ошибка: пишем без конца, а не выбрасываем.
    let end_line = children
        .last()
        .filter(|n| n.kind() == "PREPROC_ENDREGION_KEYWORD")
        .map(|n| n.start_position().row as u32 + 1);

    done.push(Region {
        name,
        line: node.start_position().row as u32 + 1,
        end_line,
    });
}

/// Комментарий-шапка: строки `//` в начале файла, до первого кода.
///
/// Полезен сам по себе — идёт в выжимку для агента вместо тела модуля,
/// а это второй критерий замены (токен-эффективность).
fn header_comment(source: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in source.lines() {
        let t = line.trim();
        if t.is_empty() && lines.is_empty() {
            continue;
        }
        if let Some(rest) = t.strip_prefix("//") {
            lines.push(rest.trim().to_string());
        } else {
            break;
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const МОДУЛЬ: &str = "// Шапка модуля.\n\
// Вторая строка.\n\
\n\
#Область Служебные\n\
\n\
Процедура ПриЗаписи(Отказ, Замещение = Ложь) Экспорт\n\
\tОбщегоНазначения.ЗначениеРеквизита(Ссылка);\n\
\tЛокальная();\n\
КонецПроцедуры\n\
\n\
#КонецОбласти\n\
\n\
Функция Сумма(А, Б)\n\
\tВозврат А + Б;\n\
КонецФункции\n";

    #[test]
    fn методы_с_параметрами_и_экспортом() {
        let m = parse(МОДУЛЬ).unwrap();
        assert_eq!(m.methods.len(), 2);
        assert_eq!(m.methods[0].name, "ПриЗаписи");
        assert_eq!(m.methods[0].kind, "Процедура");
        assert!(m.methods[0].is_export);
        assert!(
            m.methods[0].params.contains("Замещение"),
            "параметры не собраны: {}",
            m.methods[0].params
        );
        assert_eq!(m.methods[1].kind, "Функция");
        assert!(!m.methods[1].is_export);
    }

    #[test]
    fn аргументы_вызова_считаются() {
        // Нужно для сверки с сигнатурой из индекса. Вложенный вызов и запятая
        // внутри строки аргументами не являются — на этом ломается наивный
        // подсчёт по split(',').
        let м = "Процедура Т()
	Модуль.Без();
	Модуль.Один(А);
	Модуль.Строкой(\"привет\");
	Модуль.Хитрый(Ф(А, Б), \"х,у\");
КонецПроцедуры
";
        let m = parse(м).unwrap();
        let n = |имя: &str| {
            m.calls
                .iter()
                .find(|c| c.name.ends_with(имя))
                .unwrap_or_else(|| panic!("вызов {имя} не найден: {:?}", m.calls))
                .arg_count
        };
        assert_eq!(n("Без"), 0);
        assert_eq!(n("Один"), 1);
        // Строковый литерал — тоже аргумент. Поймано живым прогоном 05.09.2026:
        // `СообщитьПользователю("привет")` считался нулём, и инструмент выдавал
        // ложную тревогу «передано 0, объявлено 1..5» на совершенно верном коде.
        assert_eq!(n("Строкой"), 1, "строковый литерал не посчитан аргументом");
        assert_eq!(
            n("Хитрый"),
            2,
            "вложенные скобки или строка посчитаны как аргументы"
        );
    }

    #[test]
    fn квалификатор_вызова_не_теряется() {
        // Ровно то, что теряла прежняя реализация: без квалификатора имя
        // неотличимо от локального вызова и разрешается неверно.
        let m = parse(МОДУЛЬ).unwrap();
        let names: Vec<&str> = m.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"ОбщегоНазначения.ЗначениеРеквизита"),
            "собрано: {names:?}"
        );
        assert!(names.contains(&"Локальная"), "собрано: {names:?}");
    }

    #[test]
    fn менеджер_объекта_собирается_целиком() {
        let src = "Процедура П()\n\tСправочники.Номенклатура.НайтиПоКоду(К);\nКонецПроцедуры\n";
        let m = parse(src).unwrap();
        assert_eq!(m.calls[0].name, "Справочники.Номенклатура.НайтиПоКоду");
    }

    #[test]
    fn вызов_привязан_к_методу_владельцу() {
        let m = parse(МОДУЛЬ).unwrap();
        let c = m
            .calls
            .iter()
            .find(|c| c.name.starts_with("ОбщегоНазначения"))
            .unwrap();
        assert_eq!(c.caller, Some(0), "вызов приписан не тому методу");
    }

    #[test]
    fn области_с_границами() {
        let m = parse(МОДУЛЬ).unwrap();
        assert_eq!(m.regions.len(), 1, "области: {:?}", m.regions);
        assert_eq!(m.regions[0].name, "Служебные");
        assert!(m.regions[0].end_line.is_some(), "конец области не найден");
    }

    #[test]
    fn шапка_модуля() {
        let m = parse(МОДУЛЬ).unwrap();
        let h = m.header_comment.unwrap();
        assert!(h.starts_with("Шапка модуля."));
        assert!(h.contains("Вторая строка."));
    }

    #[test]
    fn модуль_без_шапки_не_выдумывает_её() {
        let m = parse("Процедура П()\nКонецПроцедуры\n").unwrap();
        assert!(m.header_comment.is_none());
    }

    #[test]
    fn loc_считается_по_границам() {
        let m = parse(МОДУЛЬ).unwrap();
        assert_eq!(m.methods[1].loc(), 3, "Функция Сумма занимает 3 строки");
    }

    // --- перехваты расширений ---
    //
    // Формы взяты из живого корпуса ДО (9 расширений, 13 перехватов), а не
    // выдуманы: синтетика проверяет ту форму, которую сама и придумала.

    #[test]
    fn перехват_изменение_и_контроль() {
        // Форма из ВыделениеКонтрагента: аннотация на строке 1, метод на 2.
        let src = "&ИзменениеИКонтроль(\"ДобавитьЗначение\")
Процедура Расш1_ДобавитьЗначение(Знач)
КонецПроцедуры
";
        let m = parse(src).unwrap();
        assert_eq!(m.overrides.len(), 1, "перехваты: {:?}", m.overrides);
        let o = &m.overrides[0];
        assert_eq!(o.annotation, "ИзменениеИКонтроль");
        assert_eq!(o.target_method, "ДобавитьЗначение");
        assert_eq!(o.method_name, "Расш1_ДобавитьЗначение");
        assert_eq!(o.line, 1, "адрес — строка АННОТАЦИИ, не объявления");
    }

    #[test]
    fn директива_компиляции_не_перехват() {
        // `&НаСервере` имеет ту же форму узла, но БЕЗ строкового аргумента —
        // этим и отличается. Ловилось бы списком имён, но список устареет.
        let src = "&НаСервере
Процедура П()
КонецПроцедуры
";
        let m = parse(src).unwrap();
        assert!(
            m.overrides.is_empty(),
            "директива принята за перехват: {:?}",
            m.overrides
        );
    }

    #[test]
    fn директива_перед_аннотацией_не_сбивает_адрес() {
        // Живой случай ПоказателиСотрудниковПоКПЭ: между `&НаСервере` и
        // объявлением стоит `&После(...)`. Прежний инструмент пишет строку `&После`.
        let src = "&НаСервере
&После(\"ПриСозданииНаСервере\")
Процедура РасшКПЭ_ПриСозданииНаСервере(Отказ)
КонецПроцедуры
";
        let m = parse(src).unwrap();
        assert_eq!(m.overrides.len(), 1, "перехваты: {:?}", m.overrides);
        assert_eq!(m.overrides[0].annotation, "После");
        assert_eq!(m.overrides[0].line, 2);
        assert_eq!(m.overrides[0].method_name, "РасшКПЭ_ПриСозданииНаСервере");
    }

    #[test]
    fn перехват_внутри_области_находится() {
        // Весь модуль формы у КПЭ обёрнут в `#Область` — обход обязан
        // заходить внутрь, иначе перехватов «нет» при живых трёх.
        let src = "#Область Обработчики
&После(\"ПриЗаписи\")
Процедура РасшКПЭ_ПриЗаписи(Отказ)
КонецПроцедуры
#КонецОбласти
";
        let m = parse(src).unwrap();
        assert_eq!(m.overrides.len(), 1, "перехват внутри области потерян");
    }

    #[test]
    fn вид_перехвата_не_из_списка() {
        // `Вместо` и `Перед` в корпусе ДО не встречаются — разбираются тем же
        // кодом, но живьём НЕ проверены. Тест фиксирует хотя бы форму.
        let src = "&Вместо(\"Цель\")
Функция Расш_Цель()
КонецФункции
";
        let m = parse(src).unwrap();
        assert_eq!(m.overrides.len(), 1);
        assert_eq!(m.overrides[0].annotation, "Вместо");
    }
}
