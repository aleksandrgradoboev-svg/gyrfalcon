//! Разбор BSL через tree-sitter.
//!
//! Имена узлов и полей взяты из `node-types.json` vendored-грамматики,
//! а не по памяти: `procedure_definition` / `function_definition` с полями
//! `name`, `parameters`, `export`; вызовы — `method_call` с полем `name`.

use crate::{ParseError, Result};
use tree_sitter::{Node, Parser};

extern "C" {
    fn tree_sitter_bsl() -> tree_sitter::Language;
    fn tree_sitter_sdbl() -> tree_sitter::Language;
}

/// Грамматика языка BSL.
pub fn language() -> tree_sitter::Language {
    unsafe { tree_sitter_bsl() }
}

/// Грамматика языка запросов SDBL.
pub fn language_sdbl() -> tree_sitter::Language {
    unsafe { tree_sitter_sdbl() }
}

fn parser_for(lang: tree_sitter::Language) -> Result<Parser> {
    let mut p = Parser::new();
    p.set_language(&lang)
        .map_err(|e| ParseError::Language(e.to_string()))?;
    Ok(p)
}

/// Метод модуля: процедура или функция.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub name: String,
    pub is_function: bool,
    pub is_export: bool,
    /// Строки, считая с единицы — как их показывает конфигуратор.
    pub line_start: u32,
    pub line_end: u32,
}

/// Разобрать модуль и вернуть его методы.
///
/// Ищет объявления на любой глубине: в BSL они лежат на верхнем уровне,
/// но обход по всему дереву устойчив к тому, что на битом модуле грамматика
/// может поместить объявление внутрь узла-ошибки.
pub fn parse_module(source: &str) -> Result<Vec<Method>> {
    let mut parser = parser_for(language())?;
    let tree = parser.parse(source, None).ok_or(ParseError::NoTree)?;

    let mut out = Vec::new();
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if kind == "procedure_definition" || kind == "function_definition" {
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            {
                out.push(Method {
                    name: name.to_string(),
                    is_function: kind == "function_definition",
                    is_export: node.child_by_field_name("export").is_some(),
                    line_start: node.start_position().row as u32 + 1,
                    line_end: node.end_position().row as u32 + 1,
                });
            }
        }
        stack.extend(node.children(&mut cursor));
    }

    out.sort_by_key(|m| m.line_start);
    Ok(out)
}

/// Синтаксическая ошибка, найденная парсером.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    /// Строка, считая с единицы.
    pub line: u32,
    /// Колонка, считая с единицы.
    pub column: u32,
    pub message: String,
}

/// Проверить синтаксис модуля.
///
/// Ловит **структурные** ошибки: по замеру 27.08.2026 — 7 из 8 классов при нуле
/// ложных тревог. Пропускает отсутствие точки с запятой, и по делу: в BSL
/// перевод строки разделяет операторы.
///
/// Семантику — несуществующий метод, опечатку в имени, число аргументов —
/// парсер не видит и видеть не может. Это проверка по индексу, а не по AST.
pub fn check_syntax(source: &str) -> Result<Vec<SyntaxError>> {
    let mut parser = parser_for(language())?;
    let tree = parser.parse(source, None).ok_or(ParseError::NoTree)?;

    let mut out = Vec::new();
    if !tree.root_node().has_error() {
        return Ok(out);
    }

    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        // MISSING-узлы нулевой длины: грамматика знает, чего именно не хватает.
        if node.is_missing() {
            out.push(error_at(node, format!("отсутствует {}", node.kind())));
        } else if node.is_error() {
            out.push(error_at(node, "неразобранный фрагмент".to_string()));
        }
        // В поддерево без ошибок не спускаемся — там ловить нечего.
        stack.extend(node.children(&mut cursor).filter(|c| c.has_error()));
    }

    out.sort_by_key(|e| (e.line, e.column));
    Ok(out)
}

fn error_at(node: Node, message: String) -> SyntaxError {
    let p = node.start_position();
    SyntaxError {
        line: p.row as u32 + 1,
        column: p.column as u32 + 1,
        message,
    }
}

/// Вызовы методов, встреченные в модуле: сырьё для графа вызовов.
///
/// **Это ещё не рёбра графа.** Здесь только имена в точке вызова, без разрешения
/// в фактическое определение: вызов через общий модуль и вызов через менеджер
/// объекта на этом уровне не различаются. Разрешение имён — работа индекса.
pub fn collect_calls(source: &str) -> Result<Vec<String>> {
    let mut parser = parser_for(language())?;
    let tree = parser.parse(source, None).ok_or(ParseError::NoTree)?;

    let mut out = Vec::new();
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "method_call" {
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            {
                out.push(name.to_string());
            }
        }
        stack.extend(node.children(&mut cursor));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const МОДУЛЬ: &str = "
Процедура ПриЗаписи(Отказ) Экспорт
    СообщитьПользователю(\"привет\");
КонецПроцедуры

Функция Сумма(А, Б)
    Возврат А + Б;
КонецФункции
";

    #[test]
    fn грамматика_грузится() {
        assert!(parser_for(language()).is_ok());
        assert!(parser_for(language_sdbl()).is_ok());
    }

    #[test]
    fn разбирает_процедуру_и_функцию() {
        let m = parse_module(МОДУЛЬ).unwrap();
        assert_eq!(m.len(), 2, "ожидались процедура и функция, вышло: {m:?}");

        assert_eq!(m[0].name, "ПриЗаписи");
        assert!(!m[0].is_function);
        assert!(m[0].is_export, "Экспорт не распознан");

        assert_eq!(m[1].name, "Сумма");
        assert!(m[1].is_function);
        assert!(!m[1].is_export);
    }

    #[test]
    fn строки_считаются_с_единицы() {
        let m = parse_module(МОДУЛЬ).unwrap();
        // Модуль начинается с перевода строки, объявление — на второй строке.
        assert_eq!(m[0].line_start, 2);
        assert_eq!(m[0].line_end, 4);
    }

    #[test]
    fn целый_модуль_без_ошибок() {
        assert_eq!(check_syntax(МОДУЛЬ).unwrap(), vec![]);
    }

    #[test]
    fn ловит_отсутствие_конецпроцедуры() {
        let битый = "Процедура Тест()\n    А = 1;\n";
        let e = check_syntax(битый).unwrap();
        assert!(!e.is_empty(), "незакрытая процедура не поймана");
    }

    #[test]
    fn ловит_лишнюю_скобку() {
        let битый = "Процедура Тест()\n    А = (1 + 2));\nКонецПроцедуры\n";
        let e = check_syntax(битый).unwrap();
        assert!(!e.is_empty(), "лишняя скобка не поймана");
    }

    #[test]
    fn ловит_если_без_тогда() {
        let битый =
            "Процедура Тест()\n    Если А = 1\n        Б = 2;\n    КонецЕсли;\nКонецПроцедуры\n";
        let e = check_syntax(битый).unwrap();
        assert!(!e.is_empty(), "Если без Тогда не поймано");
    }

    #[test]
    fn пропуск_точки_с_запятой_не_ошибка() {
        // В BSL перевод строки разделяет операторы — это не ошибка, и парсер прав.
        let без_точки = "Процедура Тест()\n    А = 1\n    Б = 2\nКонецПроцедуры\n";
        assert_eq!(check_syntax(без_точки).unwrap(), vec![]);
    }

    #[test]
    fn собирает_вызовы() {
        let calls = collect_calls(МОДУЛЬ).unwrap();
        assert!(
            calls.contains(&"СообщитьПользователю".to_string()),
            "вызов не найден, собрано: {calls:?}"
        );
    }

    #[test]
    fn ё_в_идентификаторе_разбирается() {
        // Правка форка ac84ac5: ё/Ё в классе символов идентификатора.
        let m = parse_module("Процедура УчётЗатрат()\nКонецПроцедуры\n").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "УчётЗатрат");
    }

    #[test]
    fn многострочное_объявление_параметров() {
        // Прежний инструмент (regex-разбор) такие объявления пропускает: заголовок процедуры
        // разорван переносом, параметры на следующих строках. На прежний инструментном корпусе
        // именно так теряются 6 процедур из 530 886 — расхождение 28.08.2026
        // разобрано поимённо и оказалось нашим преимуществом, а не ошибкой.
        let м = "&НаКлиенте\nПроцедура ДлинноеИмя(\n\tПервый,\n\tВторой,\n\tТретий) Экспорт\n\tА = 1;\nКонецПроцедуры\n";
        let ms = parse_module(м).unwrap();
        assert_eq!(ms.len(), 1, "многострочное объявление не найдено");
        assert_eq!(ms[0].name, "ДлинноеИмя");
        assert!(
            ms[0].is_export,
            "Экспорт после переноса параметров не распознан"
        );
        assert_eq!(check_syntax(м).unwrap(), vec![]);
    }

    #[test]
    fn директива_области_в_теле_цикла_ломает_разбор() {
        // ИЗВЕСТНЫЙ ДЕФЕКТ грамматики, не наш: #Область / #КонецОбласти внутри
        // тела цикла не допускаются, хотя 1С такой код компилирует. На прежний инструментном
        // корпусе это 4 файла из 18 230. Тест сторожит статус-кво: если он
        // однажды упадёт — дефект починили, и оговорку в документах надо снять.
        let битый =
            "Процедура П()\n\tДля Каждого С Из Т Цикл\n\t\tА = 1;\n#КонецОбласти\n#Область Д\n\tКонецЦикла;\nКонецПроцедуры\n";
        assert!(
            !check_syntax(битый).unwrap().is_empty(),
            "дефект починен — обновить оговорку в docs/decisions.md"
        );
    }

    #[test]
    fn директива_посреди_выражения_ломает_разбор() {
        // Второй известный дефект: #Если внутри ВЫРАЖЕНИЯ (между операторами —
        // допускается). На прежний инструментном корпусе это 1 файл. Тест сторожит статус-кво.
        let битый = "Процедура П()\n\tА = Б\n#Если ВебКлиент Тогда\n\t\t+ В\n#Иначе\n\t\t+ Г\n#КонецЕсли\n\t\t+ Д;\nКонецПроцедуры\n";
        assert!(
            !check_syntax(битый).unwrap().is_empty(),
            "дефект починен — обновить оговорку в docs/decisions.md"
        );
    }

    #[test]
    fn em_space_не_ломает_разбор() {
        // Правка форка d506854: весь класс юникодных пробелов в extras.
        // Именно U+2003 был единственным сбоем на корпусе A при замере 27.08.2026.
        let с_em = "Процедура\u{2003}Тест()\n\u{2003}А = 1;\nКонецПроцедуры\n";
        assert_eq!(
            check_syntax(с_em).unwrap(),
            vec![],
            "EM SPACE всё ещё ломает разбор — правка форка не доехала"
        );
    }
}
