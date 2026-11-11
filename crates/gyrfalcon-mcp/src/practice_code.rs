//! Добыча кандидатов практик направления «Конструкции кода» — ЭКСПЕРИМЕНТАЛЬНО.
//!
//! Перенос пилота добычи практик (Python, 08.10.2026) на Rust — дословный,
//! чтобы выдача сверялась с пилотом до единицы на тех же индексах и цифры
//! его экзамена оставались в силе. Поэтому разбор здесь свой, регулярный,
//! а не дерево tree-sitter из `gyrfalcon-parser`: перевод на дерево меняет
//! выдачу и требует повторного экзамена.
//!
//! # Что ищется
//!
//! Метод разбирается на операторы; у каждого метода — набор «символов»:
//! `ИмяСлева = литерал`, `@реквизит = …` (присваивание реквизиту объекта
//! модуля), `оператор Перейти` и прочие операторы грамматики. Дальше два
//! механизма, оба без единого имени из конкретной конфигурации:
//!
//! - **K1 избегание**: символ, который в целом встречается часто, а в
//!   контексте (вид модуля или обработчик, живущий в 50+ модулях) — почти
//!   никогда. Ожидание — по той же директиве компиляции (`K1-avoid`) или,
//!   для обработчиков, взвешенно по видам модулей (`K1-avoid-mt`).
//! - **K2 операторы грамматики**: сколько методов пользуется оператором
//!   (`K2-operator` — сводка) и избегает ли его директива (`K2-operator-avoid`).
//!
//! «Избегание» — пуассоновский хвост: наблюдение не больше 20% ожидания и
//! вероятность такого или меньшего не выше 0,001.
//!
//! Механизмы K3 (`Попытка`) и K4 (охрана в начале обработчика) пилота сюда
//! НЕ перенесены: на экзамене они не нашли ни одного правила.
//!
//! # Почему кандидаты, а не правила
//!
//! Это частота в типовом коде, а не норма. На экзамене пилота по базису
//! стандартов механизмы K1/K2 нашли 4 правила из 5 измеримых на разработочной
//! половине и 2 на отложенной (промахов среди измеренных нет, 7 правил не
//! измерены), находки воспроизвелись на второй конфигурации без настройки.
//! Отсюда выдача: кандидат с наблюдением и ожиданием, решение — за человеком.
//!
//! # Где живёт результат
//!
//! Добыча идёт командой `gyrfalcon practices mine-code` — по всей
//! конфигурации и с чтением исходников, то есть долго для вызова
//! инструмента. Результат — файл рядом с индексом
//! (`<индекс>.practices-code.json`); схема индекса не меняется. Инструмент
//! `practice_candidates` только читает этот файл.

use rayon::prelude::*;
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Формат файла кандидатов. Меняется — старые файлы честно отвергаются.
pub const FORMAT: &str = "gyrfalcon-practices-code/1";

/// Что известно о точности направления — отдаётся в каждом ответе.
pub const PRECISION: &str = "Экзамен пилота по базису стандартов 1С (08.10.2026): механизмы K1/K2 \
нашли 4 правила из 5 измеримых на разработочной половине и 2 на отложенной (промахов среди \
измеренных нет, 7 правил не измерены); находки воспроизвелись на второй конфигурации без \
настройки. Это частота в коде, а не норма: кандидат проверяет человек.";

// ---------------------------------------------------------------- лексика

fn kw_map() -> &'static HashMap<String, &'static str> {
    static M: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    M.get_or_init(|| {
        [
            ("Попытка", "TRY"),
            ("Try", "TRY"),
            ("Исключение", "EXCEPT"),
            ("Except", "EXCEPT"),
            ("КонецПопытки", "ENDTRY"),
            ("EndTry", "ENDTRY"),
            ("Если", "IF"),
            ("If", "IF"),
            ("Тогда", "THEN"),
            ("Then", "THEN"),
            ("ИначеЕсли", "ELSIF"),
            ("ElsIf", "ELSIF"),
            ("Иначе", "ELSE"),
            ("Else", "ELSE"),
            ("КонецЕсли", "ENDIF"),
            ("EndIf", "ENDIF"),
            ("Для", "FOR"),
            ("For", "FOR"),
            ("Пока", "WHILE"),
            ("While", "WHILE"),
            ("Цикл", "DO"),
            ("Do", "DO"),
            ("КонецЦикла", "ENDDO"),
            ("EndDo", "ENDDO"),
            ("Новый", "NEW"),
            ("New", "NEW"),
            ("Возврат", "RETURN"),
            ("Return", "RETURN"),
            ("ВызватьИсключение", "RAISE"),
            ("Raise", "RAISE"),
            ("И", "OP"),
            ("ИЛИ", "OP"),
            ("НЕ", "OP"),
            ("And", "OP"),
            ("Or", "OP"),
            ("Not", "OP"),
            ("Каждого", "OP"),
            ("Each", "OP"),
            ("Из", "OP"),
            ("In", "OP"),
            ("По", "OP"),
            ("To", "OP"),
            ("Процедура", "PROC"),
            ("Функция", "PROC"),
            ("КонецПроцедуры", "ENDPROC"),
            ("КонецФункции", "ENDPROC"),
            ("Знач", "OP"),
            ("Экспорт", "OP"),
            ("Перем", "OP"),
            ("Истина", "LIT"),
            ("Ложь", "LIT"),
            ("Неопределено", "LIT"),
            ("Null", "LIT"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_lowercase(), v))
        .collect()
    })
}

/// Операторы грамматики, кроме управляющих блоками: имя в нижнем регистре →
/// каноническое имя.
fn grammar(low: &str) -> Option<&'static str> {
    Some(match low {
        "прервать" | "break" => "Прервать",
        "продолжить" | "continue" => "Продолжить",
        "выполнить" | "execute" => "Выполнить",
        "добавитьобработчик" | "addhandler" => "ДобавитьОбработчик",
        "удалитьобработчик" | "removehandler" => "УдалитьОбработчик",
        "ждать" | "await" => "Ждать",
        _ => return None,
    })
}

const GRAMMAR_NAMES: &[&str] = &[
    "Прервать",
    "Продолжить",
    "Выполнить",
    "ДобавитьОбработчик",
    "УдалитьОбработчик",
    "Ждать",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    Num,
    Date,
    Id,
    P,
}

#[derive(Debug, Clone)]
struct Tok {
    kind: Kind,
    text: String,
    /// Нижний регистр — только у идентификаторов.
    low: String,
}

impl Tok {
    fn kw(&self) -> Option<&'static str> {
        if self.kind == Kind::Id {
            kw_map().get(&self.low).copied()
        } else {
            None
        }
    }
}

/// Снять комментарии `//` вне строк и строки препроцессора/директив.
///
/// Кавычка просто переключает «внутри строки»: удвоенная кавычка — два
/// переключения, итог тот же. Ровно как в пилоте.
pub fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let s = line.trim_start();
        if s.starts_with('#') || s.starts_with('&') {
            continue;
        }
        let mut q = false;
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'"' {
                q = !q;
            } else if !q && b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
                break;
            }
            i += 1;
        }
        out.push_str(&line[..i]);
    }
    out
}

fn id_start(c: char) -> bool {
    c.is_ascii_alphabetic() || ('А'..='я').contains(&c) || c == 'Ё' || c == 'ё' || c == '_'
}

fn id_cont(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Тело строкового литерала после открывающего символа: всё до кавычки или
/// перевода строки, удвоенная кавычка — часть строки, закрывающая — по
/// желанию. Возвращает позицию за концом.
fn str_body(c: &[char], mut j: usize) -> usize {
    while j < c.len() {
        match c[j] {
            '"' if c.get(j + 1) == Some(&'"') => j += 2,
            '"' => return j + 1,
            '\n' => return j,
            _ => j += 1,
        }
    }
    j
}

/// Лексемы — те же, что у регулярного разборщика пилота, в том же порядке
/// альтернатив: строка, продолжение многострочной строки (`|` в начале
/// строки), число, дата, идентификатор, знак. Прочее пропускается.
fn tokens(text: &str) -> Vec<Tok> {
    let c: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let push = |out: &mut Vec<Tok>, kind, s: String| {
        let low = if kind == Kind::Id {
            s.to_lowercase()
        } else {
            String::new()
        };
        out.push(Tok { kind, text: s, low });
    };
    while i < c.len() {
        let ch = c[i];
        if ch == '"' {
            let e = str_body(&c, i + 1);
            push(&mut out, Kind::Str, c[i..e].iter().collect());
            i = e;
            continue;
        }
        if i == 0 || c[i - 1] == '\n' {
            let mut j = i;
            while j < c.len() && c[j].is_whitespace() {
                j += 1;
            }
            if j < c.len() && c[j] == '|' {
                let e = str_body(&c, j + 1);
                push(&mut out, Kind::Str, c[i..e].iter().collect());
                i = e;
                continue;
            }
        }
        if ch.is_ascii_digit() {
            let mut j = i;
            while j < c.len() && c[j].is_ascii_digit() {
                j += 1;
            }
            if j + 1 < c.len() && c[j] == '.' && c[j + 1].is_ascii_digit() {
                j += 1;
                while j < c.len() && c[j].is_ascii_digit() {
                    j += 1;
                }
            }
            push(&mut out, Kind::Num, c[i..j].iter().collect());
            i = j;
            continue;
        }
        if ch == '\'' {
            let mut j = i + 1;
            while j < c.len() && c[j] != '\'' && c[j] != '\n' {
                j += 1;
            }
            if j < c.len() && c[j] == '\'' {
                push(&mut out, Kind::Date, c[i..=j].iter().collect());
                i = j + 1;
                continue;
            }
            i += 1;
            continue;
        }
        if id_start(ch) {
            let mut j = i + 1;
            while j < c.len() && id_cont(c[j]) {
                j += 1;
            }
            push(&mut out, Kind::Id, c[i..j].iter().collect());
            i = j;
            continue;
        }
        if "().,;=<>+-*/%[]?".contains(ch) {
            push(&mut out, Kind::P, ch.to_string());
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------- операторы

/// Оператор метода — только то, из чего строятся символы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `lhs` нормализован (`Имя`, `Имя.*`, `param:Имя`); `rhs` — `=литерал`
    /// в нижнем регистре, `N`, `S`, `I` или `E`.
    Assign {
        lhs: String,
        rhs: String,
    },
    Goto,
    Kw(&'static str),
}

/// Разобранный метод: нужные операторы и общее число операторов (как
/// считает пилот — с условиями, вызовами, возвратами и `Попытка`).
#[derive(Debug, Default)]
pub struct Parsed {
    pub stmts: Vec<Stmt>,
    pub total: usize,
}

fn has_call(ts: &[Tok]) -> bool {
    ts.windows(2)
        .any(|w| w[0].kind == Kind::Id && w[1].text == "(" && w[0].kw().is_none())
}

/// Классифицировать оператор. `None` в первом — не нужен механизмам;
/// второй — засчитан ли он вообще как оператор.
fn classify(ts: &[Tok], params: &HashSet<String>) -> (Option<Stmt>, bool) {
    let t0 = &ts[0];
    match t0.kw() {
        Some("RETURN") | Some("RAISE") => return (None, true),
        _ => {}
    }
    if t0.kind == Kind::Id {
        if t0.low == "перейти" || t0.low == "goto" {
            return (Some(Stmt::Goto), true);
        }
        if let Some(g) = grammar(&t0.low) {
            return (Some(Stmt::Kw(g)), true);
        }
    }
    let mut i = 0;
    let mut chain: Vec<&str> = Vec::new();
    while i < ts.len() && ts[i].kind == Kind::Id && ts[i].kw().is_none() {
        chain.push(&ts[i].text);
        i += 1;
        if i < ts.len() && ts[i].text == "." {
            i += 1;
        } else {
            break;
        }
    }
    if !chain.is_empty() && i < ts.len() && ts[i].text == "=" {
        let rhs_t = &ts[i + 1..];
        let rhs = if rhs_t.len() == 1 {
            let t = &rhs_t[0];
            if t.kind == Kind::Id && t.kw() == Some("LIT") {
                format!("={}", t.low)
            } else {
                match t.kind {
                    Kind::Num | Kind::Date => "N".into(),
                    Kind::Str => "S".into(),
                    _ => "I".into(),
                }
            }
        } else {
            "E".into()
        };
        let tail = if chain.len() > 1 { ".*" } else { "" };
        let lhs = if params.contains(&chain[0].to_lowercase()) {
            format!("param:{}{tail}", chain[0])
        } else {
            format!("{}{tail}", chain[0])
        };
        return (Some(Stmt::Assign { lhs, rhs }), true);
    }
    (None, has_call(ts))
}

/// Разобрать тело метода на операторы.
pub fn parse(body: &str, params: &HashSet<String>) -> Parsed {
    let toks = tokens(&strip_comments(body));
    let mut p = Parsed::default();
    let mut cur: Vec<Tok> = Vec::new();
    let mut try_depth = 0usize;
    let flush = |cur: &mut Vec<Tok>, p: &mut Parsed| {
        if cur.is_empty() {
            return;
        }
        let (st, counted) = classify(cur, params);
        if counted {
            p.total += 1;
        }
        if let Some(s) = st {
            p.stmts.push(s);
        }
        cur.clear();
    };
    let n = toks.len();
    let mut i = 0;
    while i < n {
        let t = &toks[i];
        match t.kw() {
            Some(k @ ("IF" | "ELSIF" | "WHILE")) => {
                flush(&mut cur, &mut p);
                let mut j = i + 1;
                while j < n && !matches!(toks[j].kw(), Some("THEN") | Some("DO")) {
                    j += 1;
                }
                if k != "WHILE" {
                    p.total += 1;
                }
                i = j + 1;
                continue;
            }
            Some("FOR") => {
                flush(&mut cur, &mut p);
                let mut j = i;
                while j < n && toks[j].kw() != Some("DO") {
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            Some("ELSE" | "ENDIF" | "DO" | "ENDDO" | "EXCEPT") => flush(&mut cur, &mut p),
            Some("TRY") => {
                flush(&mut cur, &mut p);
                try_depth += 1;
            }
            Some("ENDTRY") => {
                flush(&mut cur, &mut p);
                if try_depth > 0 {
                    try_depth -= 1;
                    p.total += 1;
                }
            }
            _ if t.text == ";" && t.kind == Kind::P => flush(&mut cur, &mut p),
            _ => cur.push(t.clone()),
        }
        i += 1;
    }
    flush(&mut cur, &mut p);
    p
}

/// Параметры метода из строки индекса `(Знач А, Б = 1)` → имена в нижнем
/// регистре.
fn param_names(params: Option<&str>) -> HashSet<String> {
    let s = params.unwrap_or("").trim_matches(|c| c == '(' || c == ')');
    let mut out = HashSet::new();
    for p in s.split(',') {
        let mut p = p.trim_start();
        let low = p.to_lowercase();
        if low.starts_with("знач") && low[("знач".len())..].starts_with(char::is_whitespace)
        {
            p = p["знач".len()..].trim_start();
        }
        let name = p.split('=').next().unwrap_or("").trim().to_lowercase();
        if !name.is_empty() {
            out.insert(name);
        }
    }
    out
}

// ---------------------------------------------------------------- корпус

/// Привести переводы строк к `\n`, считая одиночный `\r` переводом строки.
fn newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

struct Module {
    category: Option<String>,
    object: Option<String>,
    module_type: Option<String>,
    is_form: bool,
}

struct MethodRec {
    module: i64,
    name: String,
    directive: String,
    stmts: Vec<Stmt>,
}

/// Итог добычи.
pub struct Mined {
    pub methods: usize,
    pub statements: usize,
    pub records: Vec<Value>,
}

/// Корпус: модули по id, методы в порядке пилота, общее число операторов.
type Corpus = (HashMap<i64, Module>, Vec<MethodRec>, usize);

/// Прочитать методы конфигурации и разобрать их на операторы.
fn corpus(conn: &Connection, source: &Path) -> Result<Corpus, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let mut st = conn
        .prepare("SELECT id, rel_path, category, object_name, module_type, is_form FROM modules")
        .map_err(e)?;
    #[allow(clippy::type_complexity)]
    let mods: Vec<(
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        bool,
    )> = st
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get::<_, Option<i64>>(5)?.unwrap_or(0) != 0,
            ))
        })
        .map_err(e)?
        .collect::<Result<_, _>>()
        .map_err(e)?;
    type Meth = (i64, String, Option<String>, Option<i64>, Option<i64>);
    let mut by_module: HashMap<i64, Vec<Meth>> = HashMap::new();
    let mut st = conn
        .prepare("SELECT id, module_id, name, params, line, end_line FROM methods")
        .map_err(e)?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(1)?,
                (
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                ),
            ))
        })
        .map_err(e)?;
    for r in rows {
        let (m, rec) = r.map_err(e)?;
        by_module.entry(m).or_default().push(rec);
    }

    // Модули разбираются параллельно, порядок методов — как у пилота:
    // модули в порядке таблицы, методы внутри — в порядке таблицы.
    let parsed: Vec<Option<(Module, Vec<MethodRec>, usize)>> = mods
        .par_iter()
        .map(|(id, rel, cat, obj, mt, is_form)| {
            let bytes = std::fs::read(source.join(rel)).ok()?;
            let text = String::from_utf8(bytes).ok()?;
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            // Перевод строки — любой из `\r\n`, `\n` и одиночного `\r`: так
            // нумерует строки индекс. Одиночный `\r` встречается в типовых
            // (найден сверкой с пилотом: модуль съезжал на строку, и тело
            // каждого метода бралось со сдвигом).
            let text = newlines(text);
            let lines: Vec<&str> = text.split('\n').collect();
            let mut recs = Vec::new();
            let mut total = 0usize;
            for (_, name, params, line, end) in by_module.get(id).into_iter().flatten() {
                let (line, end) = match (line, end) {
                    (Some(l), Some(e)) if *l != 0 && *e != 0 => (*l as usize, *e as usize),
                    _ => continue,
                };
                let mut directive = String::new();
                let mut k = line as i64 - 2;
                while k >= 0 && (k as usize) < lines.len() {
                    let s = lines[k as usize].trim();
                    if !(s.starts_with('&') || s.starts_with("//")) {
                        break;
                    }
                    if s.starts_with('&') {
                        directive = s.to_string();
                        break;
                    }
                    k -= 1;
                }
                let from = line.min(lines.len());
                let to = end.saturating_sub(1).clamp(from, lines.len());
                let body = lines[from..to].join("\n");
                let p = parse(&body, &param_names(params.as_deref()));
                total += p.total;
                recs.push(MethodRec {
                    module: *id,
                    name: name.clone(),
                    directive,
                    stmts: p.stmts,
                });
            }
            Some((
                Module {
                    category: cat.clone(),
                    object: obj.clone(),
                    module_type: mt.clone(),
                    is_form: *is_form,
                },
                recs,
                total,
            ))
        })
        .collect();
    let mut modules = HashMap::new();
    let mut methods = Vec::new();
    let mut statements = 0;
    for ((id, ..), r) in mods.iter().zip(parsed) {
        if let Some((m, recs, total)) = r {
            modules.insert(*id, m);
            methods.extend(recs);
            statements += total;
        }
    }
    Ok((modules, methods, statements))
}

// ---------------------------------------------------------------- механизмы

fn pois_le(o: u64, e: f64) -> f64 {
    let mut t = (-e).exp();
    let mut acc = t;
    for k in 1..=o {
        t *= e / k as f64;
        acc += t;
    }
    acc
}

/// Наблюдение подозрительно мало для ожидания.
pub fn avoid(o: u64, e: f64) -> bool {
    e > 0.0 && (o as f64) <= 0.2 * e && pois_le(o, e) <= 0.001
}

fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Счётчик, помнящий порядок первой встречи ключа — для равенств в
/// «самом частом», как у `Counter.most_common` пилота.
#[derive(Default)]
struct Counter {
    n: HashMap<String, (u64, usize)>,
}

impl Counter {
    fn add(&mut self, k: &str) {
        let next = self.n.len();
        self.n.entry(k.to_string()).or_insert((0, next)).0 += 1;
    }
    fn get(&self, k: &str) -> u64 {
        self.n.get(k).map_or(0, |v| v.0)
    }
    fn most_common(&self) -> Option<&str> {
        self.n
            .iter()
            .max_by(|a, b| a.1 .0.cmp(&b.1 .0).then(b.1 .1.cmp(&a.1 .1)))
            .map(|(k, _)| k.as_str())
    }
}

fn opt(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("None")
}

/// Добыть кандидатов по индексу и исходникам конфигурации.
pub fn mine(conn: &Connection, source: &Path) -> Result<Mined, String> {
    let (mods, meths, statements) = corpus(conn, source)?;

    let mut attrs: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let mut st = conn
        .prepare("SELECT object_name, category, attr_name FROM object_attributes")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for r in rows {
        let (o, c, a) = r.map_err(|e| e.to_string())?;
        attrs
            .entry((
                o.unwrap_or_else(|| "None".into()),
                c.unwrap_or_else(|| "None".into()),
            ))
            .or_default()
            .insert(a.unwrap_or_default().to_lowercase());
    }

    let mut name_mods: HashMap<&str, HashSet<i64>> = HashMap::new();
    for m in &meths {
        name_mods.entry(&m.name).or_default().insert(m.module);
    }
    let handlers: HashSet<&str> = name_mods
        .iter()
        .filter(|(_, s)| s.len() >= 50)
        .map(|(n, _)| *n)
        .collect();

    let empty = HashSet::new();
    let syms = |m: &MethodRec| -> HashSet<String> {
        let md = &mods[&m.module];
        let own = if md.is_form {
            &empty
        } else {
            attrs
                .get(&(opt(&md.object).to_string(), opt(&md.category).to_string()))
                .unwrap_or(&empty)
        };
        let mut ss = HashSet::new();
        for s in &m.stmts {
            match s {
                Stmt::Assign { lhs, rhs } => {
                    let head = lhs.split('.').next().unwrap_or("").to_lowercase();
                    if own.contains(&head) || lhs.starts_with("ЭтотОбъект.") {
                        ss.insert("@реквизит = …".to_string());
                    }
                    if let Some(lit) = rhs.strip_prefix('=') {
                        ss.insert(format!("{lhs} = {lit}"));
                    }
                }
                Stmt::Goto => {
                    ss.insert("оператор Перейти".into());
                }
                Stmt::Kw(k) => {
                    ss.insert(format!("оператор {k}"));
                }
            }
        }
        ss
    };

    let form = |isf: bool| if isf { "/form" } else { "" };
    let mut n_a = Counter::default();
    let mut n_a_dir: HashMap<(String, String), u64> = HashMap::new();
    let mut n_dir = Counter::default();
    let mut n_k = Counter::default();
    let mut kdir: HashMap<String, Counter> = HashMap::new();
    let mut o_ak: HashMap<(String, String), u64> = HashMap::new();
    let mut n_mt: HashMap<String, u64> = HashMap::new();
    let mut n_amt: HashMap<(String, String), u64> = HashMap::new();
    let mut kmt: HashMap<String, Counter> = HashMap::new();
    for m in &meths {
        let ss = syms(m);
        let md = &mods[&m.module];
        let dr = match m.directive.split('(').next().unwrap_or("") {
            "" => "&<нет>".to_string(),
            d => d.to_string(),
        };
        let mt = format!(
            "mt:{}/{}{}",
            opt(&md.category),
            opt(&md.module_type),
            form(md.is_form)
        );
        let mut ks = vec![mt.clone()];
        if handlers.contains(m.name.as_str()) {
            ks.push(format!("h:{}{}", m.name, form(md.is_form)));
        }
        *n_mt.entry(mt.clone()).or_default() += 1;
        for k in &ks {
            kmt.entry(k.clone()).or_default().add(&mt);
        }
        for a in &ss {
            *n_amt.entry((a.clone(), mt.clone())).or_default() += 1;
        }
        n_dir.add(&dr);
        for k in &ks {
            n_k.add(k);
            kdir.entry(k.clone()).or_default().add(&dr);
        }
        for a in &ss {
            n_a.add(a);
            *n_a_dir.entry((a.clone(), dr.clone())).or_default() += 1;
            for k in &ks {
                *o_ak.entry((a.clone(), k.clone())).or_default() += 1;
            }
        }
    }
    let nad = |a: &str, d: &str| *n_a_dir.get(&(a.to_string(), d.to_string())).unwrap_or(&0);

    let mut out = Vec::new();
    let frequent: Vec<&String> = n_a
        .n
        .iter()
        .filter(|(_, v)| v.0 >= 30)
        .map(|(a, _)| a)
        .collect();
    for (k, &(nk, _)) in &n_k.n {
        let Some(dr) = kdir[k].most_common() else {
            continue;
        };
        for a in &frequent {
            let e = nk as f64 * nad(a, dr) as f64 / n_dir.get(dr) as f64;
            let o = *o_ak.get(&((*a).clone(), k.clone())).unwrap_or(&0);
            if avoid(o, e) {
                out.push(json!({"mech": "K1-avoid", "A": a, "K": k, "observed": o,
                    "expected": r1(e), "nK": nk}));
            } else if k.starts_with("h:") {
                let e2: f64 = kmt[k]
                    .n
                    .iter()
                    .filter(|(mt, _)| mt.starts_with("mt:"))
                    .map(|(mt, &(cnt, _))| {
                        cnt as f64 * *n_amt.get(&((*a).clone(), mt.clone())).unwrap_or(&0) as f64
                            / n_mt[mt] as f64
                    })
                    .sum();
                if avoid(o, e2) {
                    out.push(json!({"mech": "K1-avoid-mt", "A": a, "K": k, "observed": o,
                        "expected": r1(e2), "nK": nk}));
                }
            }
        }
    }

    let mut ops: Vec<String> = GRAMMAR_NAMES
        .iter()
        .map(|g| format!("оператор {g}"))
        .collect();
    ops.sort();
    ops.dedup();
    ops.insert(0, "оператор Перейти".into());
    let total = meths.len() as u64;
    for a in &ops {
        let na = n_a.get(a);
        let mut by_dir = Map::new();
        for dr in n_dir.n.keys() {
            let c = nad(a, dr);
            if c > 0 {
                by_dir.insert(dr.clone(), json!(c));
            }
        }
        out.push(
            json!({"mech": "K2-operator", "A": a, "methods": na, "of": total,
            "by_dir": by_dir}),
        );
        for (dr, &(nd, _)) in &n_dir.n {
            if nd < 500 || na < 30 {
                continue;
            }
            let o = nad(a, dr);
            let e = nd as f64 * (na - o) as f64 / (total.saturating_sub(nd)).max(1) as f64;
            if avoid(o, e) {
                out.push(
                    json!({"mech": "K2-operator-avoid", "A": a, "K": format!("dir:{dr}"),
                    "observed": o, "expected": r1(e)}),
                );
            }
        }
    }
    out.sort_by_key(|r| {
        (
            r["mech"].as_str().unwrap_or("").to_string(),
            r["A"].as_str().unwrap_or("").to_string(),
            r["K"].as_str().unwrap_or("").to_string(),
        )
    });
    Ok(Mined {
        methods: meths.len(),
        statements,
        records: out,
    })
}

// ---------------------------------------------------------------- файл

/// Файл кандидатов рядом с индексом: `<имя индекса>.practices-code.json`.
pub fn sidecar(db: &Path) -> PathBuf {
    let stem = db.file_stem().and_then(|s| s.to_str()).unwrap_or("index");
    db.with_file_name(format!("{stem}.practices-code.json"))
}

fn meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .ok()
}

/// Добыть и записать файл кандидатов. Возвращает путь и сводку.
pub fn mine_to_file(db: &Path, out: Option<&Path>) -> Result<(PathBuf, Value), String> {
    let conn = crate::sql::open_readonly(db).map_err(|e| e.to_string())?;
    let source = meta(&conn, "source_path").ok_or("в индексе нет source_path")?;
    let t0 = std::time::Instant::now();
    let m = mine(&conn, Path::new(&source))?;
    let secs = (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    let count = |mech: &str| m.records.iter().filter(|r| r["mech"] == mech).count();
    let summary = json!({
        "methods": m.methods,
        "statements": m.statements,
        "K1-avoid": count("K1-avoid"),
        "K1-avoid-mt": count("K1-avoid-mt"),
        "K2-operator-avoid": count("K2-operator-avoid"),
        "seconds": secs,
    });
    let doc = json!({
        "format": FORMAT,
        "direction": "code",
        "experimental": true,
        "generated_by": env!("CARGO_PKG_VERSION"),
        "index_built_at": meta(&conn, "built_at"),
        "source_path": source,
        "summary": summary,
        "records": m.records,
    });
    let path = out.map(Path::to_path_buf).unwrap_or_else(|| sidecar(db));
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    // Через временный файл: работающий сервер может читать файл в эту же
    // минуту, и наполовину записанный JSON стал бы для агента ошибкой.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("не записан {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("не записан {}: {e}", path.display()))?;
    Ok((path, summary))
}

// ---------------------------------------------------------------- инструмент

/// Схема инструмента `practice_candidates`.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "mech": {"type": "string", "enum": ["K1", "K2"],
                "description": "K1 — присваивания, которых избегают в контексте; K2 — операторы грамматики"},
            "contains": {"type": "string",
                "description": "фильтр по символу или контексту, без учёта регистра (например ПриЗаписи)"},
            "limit": {"type": "integer", "default": 50}
        }
    })
}

/// Отдать кандидатов из файла рядом с индексом.
pub fn candidates(conn: &Connection, args: &Value) -> Result<Value, String> {
    let db = conn
        .path()
        .filter(|p| !p.is_empty())
        .ok_or("индекс без файла: кандидатам негде лежать")?;
    let path = sidecar(Path::new(db));
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "кандидатов нет: добыча по этому индексу не запускалась. Это не значит, что \
             практик нет. Запуск: gyrfalcon practices mine-code --db {db}"
        )
    })?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if doc["format"] != FORMAT {
        return Err(format!(
            "файл {} другого формата ({}), нужен {FORMAT} — перезапустите practices mine-code",
            path.display(),
            doc["format"]
        ));
    }
    let mech = args.get("mech").and_then(Value::as_str);
    let needle = args
        .get("contains")
        .and_then(Value::as_str)
        .map(str::to_lowercase);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 1000) as usize;
    let recs = doc["records"].as_array().cloned().unwrap_or_default();
    let fits = |r: &Value| {
        needle.as_ref().is_none_or(|n| {
            r["A"].as_str().unwrap_or("").to_lowercase().contains(n)
                || r["K"].as_str().unwrap_or("").to_lowercase().contains(n)
        })
    };
    let mut rows = Vec::new();
    let mut operators = Vec::new();
    for r in &recs {
        let m = r["mech"].as_str().unwrap_or("");
        if m == "K2-operator" {
            if mech.is_none_or(|x| x == "K2") && fits(r) {
                operators.push(json!([r["A"], r["methods"], r["of"]]));
            }
            continue;
        }
        let family = if m.starts_with("K1") { "K1" } else { "K2" };
        if mech.is_some_and(|x| x != family) || !fits(r) {
            continue;
        }
        rows.push(json!([m, r["A"], r["K"], r["observed"], r["expected"]]));
    }
    let total = rows.len();
    rows.truncate(limit);
    let built_now = meta(conn, "built_at");
    // Не `stale`: это имя сервер занимает признаком свежести индекса и
    // перезаписывает (поймано живой пробой).
    let outdated = doc["index_built_at"].as_str() != built_now.as_deref();
    Ok(json!({
        "experimental": true,
        "kind": "кандидаты, а не правила",
        "precision": PRECISION,
        "read_as": "A почти не встречается в контексте K: observed — сколько методов контекста \
                    его содержат, expected — сколько ожидалось по остальному коду. Повод \
                    проверить по стандартам (standards_1c), а не норма.",
        "mined_by": doc["generated_by"],
        "outdated": outdated,
        "outdated_note": if outdated { json!("индекс пересобран после добычи — кандидаты могли устареть") } else { Value::Null },
        "columns": ["mech", "A", "K", "observed", "expected"],
        "rows": rows,
        "returned": rows.len(),
        "total": total,
        "truncated": total > limit,
        "operators_columns": ["operator", "methods", "of"],
        "operators": operators,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_lowercase()).collect()
    }

    #[test]
    fn комментарии_снимаются_вне_строк() {
        let t = strip_comments("А = \"http://x\"; // хвост\n&НаСервере\n#Если Сервер Тогда");
        assert_eq!(t, "А = \"http://x\"; \n\n");
    }

    #[test]
    fn лексемы_строки_даты_числа() {
        let t = tokens("А = \"a\"\"b\" + '20200101' + 1.5;\n\t|продолжение\nБ");
        let kinds: Vec<Kind> = t.iter().map(|x| x.kind).collect();
        use Kind::*;
        assert_eq!(kinds, vec![Id, P, Str, P, Date, P, Num, P, Str, Id]);
        assert_eq!(t[2].text, "\"a\"\"b\"");
        assert_eq!(t[6].text, "1.5");
    }

    #[test]
    fn присваивания_литерала_и_реквизита() {
        let p = parse(
            "Отказ = Ложь;\nЭтотОбъект.Сумма = 1;\nПараметр.Поле = Истина;\nХ = А + Б;",
            &ps(&["Параметр"]),
        );
        assert_eq!(
            p.stmts,
            vec![
                Stmt::Assign {
                    lhs: "Отказ".into(),
                    rhs: "=ложь".into()
                },
                Stmt::Assign {
                    lhs: "ЭтотОбъект.*".into(),
                    rhs: "N".into()
                },
                Stmt::Assign {
                    lhs: "param:Параметр.*".into(),
                    rhs: "=истина".into()
                },
                Stmt::Assign {
                    lhs: "Х".into(),
                    rhs: "E".into()
                },
            ]
        );
        assert_eq!(p.total, 4);
    }

    #[test]
    fn операторы_грамматики_и_условия() {
        let p = parse(
            "Если А = 1 Тогда\n Перейти ~М;\nИначе\n Прервать;\nКонецЕсли;\nПока Истина Цикл Продолжить; КонецЦикла;\nПопытка Х(); Исключение КонецПопытки;",
            &HashSet::new(),
        );
        assert_eq!(
            p.stmts,
            vec![Stmt::Goto, Stmt::Kw("Прервать"), Stmt::Kw("Продолжить")]
        );
        // Если, Перейти, Прервать, Продолжить, вызов Х(), Попытка.
        assert_eq!(p.total, 6);
    }

    #[test]
    fn одиночный_cr_это_перевод_строки() {
        // Строки нумеруются как в индексе: иначе модуль съезжает на строку.
        let t = newlines("А\r\nБ\rВ\nГ");
        assert_eq!(t.split('\n').collect::<Vec<_>>(), ["А", "Б", "В", "Г"]);
    }

    #[test]
    fn знач_снимается_с_параметра() {
        let s = param_names(Some("(Знач Отказ, СтандартнаяОбработка = Истина)"));
        assert_eq!(s, ps(&["Отказ", "СтандартнаяОбработка"]));
    }

    #[test]
    fn избегание_по_пуассону() {
        assert!(avoid(0, 52.0));
        assert!(!avoid(10, 20.0), "не меньше 20% ожидания");
        assert!(!avoid(0, 3.0), "слишком мало ожидания для вывода");
    }

    /// Мини-конфигурация: обработчик `ПередЗаписью` в 60 модулях без
    /// `Отказ = Ложь`, а прочие методы — с ним.
    fn корпус(dir: &Path) -> Connection {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let db = dir.join("t-index.db");
        let _ = std::fs::remove_file(&db);
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE modules (id INTEGER PRIMARY KEY, rel_path TEXT, category TEXT,
               object_name TEXT, module_type TEXT, is_form INTEGER);
             CREATE TABLE methods (id INTEGER PRIMARY KEY, module_id INTEGER, name TEXT,
               params TEXT, line INTEGER, end_line INTEGER);
             CREATE TABLE object_attributes (object_name TEXT, category TEXT, attr_name TEXT);
             CREATE TABLE index_meta (key TEXT, value TEXT);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO index_meta VALUES ('source_path', ?1), ('built_at', '1')",
            [src.to_str().unwrap()],
        )
        .unwrap();
        let mut mid = 0;
        for i in 0..60 {
            mid += 1;
            let rel = format!("m{i}.bsl");
            std::fs::write(
                src.join(&rel),
                "Процедура ПередЗаписью(Отказ)\n\tХ = 1;\nКонецПроцедуры\n\
                 Процедура Прочее(Отказ)\n\tОтказ = Ложь;\nКонецПроцедуры\n\
                 Процедура Ещё(Отказ)\n\tОтказ = Ложь;\nКонецПроцедуры\n",
            )
            .unwrap();
            c.execute(
                "INSERT INTO modules VALUES (?1, ?2, 'Document', ?3, 'ObjectModule', 0)",
                rusqlite::params![mid, rel, format!("Д{i}")],
            )
            .unwrap();
            for (name, line, end) in [("ПередЗаписью", 1, 3), ("Прочее", 4, 6), ("Ещё", 7, 9)]
            {
                c.execute(
                    "INSERT INTO methods (module_id, name, params, line, end_line)
                     VALUES (?1, ?2, '(Отказ)', ?3, ?4)",
                    rusqlite::params![mid, name, line, end],
                )
                .unwrap();
            }
        }
        c
    }

    #[test]
    fn находит_избегание_в_обработчике() {
        let dir = std::env::temp_dir().join("gyrfalcon-practice-code-mine");
        let _ = std::fs::remove_dir_all(&dir);
        let c = корпус(&dir);
        let m = mine(&c, &dir.join("src")).unwrap();
        assert_eq!(m.methods, 180);
        let hit = m
            .records
            .iter()
            .find(|r| r["A"] == "param:Отказ = ложь" && r["K"] == "h:ПередЗаписью")
            .unwrap_or_else(|| panic!("нет кандидата: {:#?}", m.records));
        assert_eq!(hit["mech"], "K1-avoid");
        assert_eq!(hit["observed"], 0);
        assert_eq!(hit["expected"], 40.0);
    }

    #[test]
    fn инструмент_читает_файл_и_говорит_о_пропуске() {
        let dir = std::env::temp_dir().join("gyrfalcon-practice-code-tool");
        let _ = std::fs::remove_dir_all(&dir);
        let c = корпус(&dir);
        let db = dir.join("t-index.db");
        let нет = candidates(&c, &json!({})).unwrap_err();
        assert!(нет.contains("не запускалась"), "{нет}");
        let (path, _) = mine_to_file(&db, None).unwrap();
        assert_eq!(path, dir.join("t-index.practices-code.json"));
        let v = candidates(&c, &json!({"contains": "передзаписью"})).unwrap();
        assert_eq!(v["experimental"], true);
        assert_eq!(v["outdated"], false);
        assert!(
            v["rows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r[2] == "h:ПередЗаписью"),
            "{v:#}"
        );
        c.execute("UPDATE index_meta SET value='2' WHERE key='built_at'", [])
            .unwrap();
        let v = candidates(&c, &json!({})).unwrap();
        assert_eq!(
            v["outdated"], true,
            "пересобранный индекс — кандидаты устарели"
        );
    }
}
