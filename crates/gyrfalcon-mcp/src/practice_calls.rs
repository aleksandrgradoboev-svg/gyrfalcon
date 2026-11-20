//! Добыча кандидатов практик направления «Вызовы и связки» — ЭКСПЕРИМЕНТАЛЬНО.
//!
//! Перенос пилота добычи практик (Python, 08.10.2026: `extract_calls.py`,
//! `mine_c1.py`, `mine_c2_c5.py`, `rank.py`) на Rust — дословный, чтобы
//! выдача сверялась с пилотом на тех же индексах и цифры его экзамена
//! оставались в силе. Разбор — тот же регулярный, что у «Конструкций кода»
//! (`practice_code`), а не дерево `gyrfalcon-parser`.
//!
//! # Что ищется
//!
//! Метод разбирается на события вызова: имя (`Модуль.Метод` для общих
//! модулей, `*.Метод` для метода переменной, `Новый Тип`), контекст блоков
//! (тело и исключение `Попытка`, цикл, условие, ветка), вид аргументов,
//! склейка через `+` и вложенность «аргумент какого вызова». Дальше шесть
//! общих механизмов, ни одного имени вызова в коде:
//!
//! - **C1 связки**: «если в методе есть A, то есть и B» с отношением
//!   (порядок, тело/исключение одной `Попытка`, цикл);
//! - **C2 контекст**: A избегают или требуют в обработчике, виде модуля,
//!   директиве, внутри блока или скобки C1;
//! - **C3 вытеснение**: платформенный метод почти всегда зовут через обёртку
//!   общего модуля (`C3-wrapper`) или вместо него зовут похожий по имени и
//!   намного более частый (`C3-similar` — **слабое основание**: сходство слов
//!   в именах, а не доказательство замены; в выдаче помечено);
//! - **C4 форма аргумента** и ключи форматных строк;
//! - **C5 граф**: доля вызываемых неэкспортных методов и экспортных, зовущихся
//!   только из своего объекта;
//! - **C6 склейка и вложенность**.
//!
//! Ранжирование — как в пилоте: значимость × разброс по модулям × отличие ×
//! сила эффекта; находки одного механизма об одном символе — семейство.
//!
//! # Почему кандидаты, а не правила
//!
//! Экзамен пилота по базису стандартов (08.10.2026): разработочная половина
//! 64%, отложенная 41% (переобучение), перенос на БП без настройки 75%
//! (строго 69%) — по правилам, которые код соблюдает. Это частота в типовом
//! коде, а не норма: решение — за человеком.
//!
//! # Где живёт результат
//!
//! Добыча — командой `gyrfalcon practices mine-calls`, результат — файл рядом
//! с индексом (`<индекс>.practices-calls.json`); схема индекса не меняется.
//! Инструмент `practice_candidates` с `direction: "calls"` только читает файл.

use crate::practice_code::{avoid, meta, newlines, param_names, strip_comments, tokens, Kind, Tok};
use rayon::prelude::*;
use regex::Regex;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Формат файла кандидатов. Меняется — старые файлы честно отвергаются.
pub const FORMAT: &str = "gyrfalcon-practices-calls/1";

/// Что известно о точности направления — отдаётся в каждом ответе.
pub const PRECISION: &str = "Экзамен пилота по базису стандартов 1С (08.10.2026), направление \
«Вызовы и связки»: разработочная половина 64%, отложенная 41% (переобучение), перенос на вторую \
конфигурацию без настройки 75% (строго 69%) — по правилам, которые код соблюдает. Это частота в \
коде, а не норма: кандидат проверяет человек.";

/// Пометка механизма со слабым основанием.
pub const WEAK_C3_SIMILAR: &str = "C3-similar — сходство слов в именах, а не доказательство \
замены: на пилоте дал и верные находки (ТекущаяДата → ТекущаяДатаСеанса), и шум";

// ---------------------------------------------------------------- округление

/// Округление как `round(x, nd)` у Python: по точному десятичному значению
/// двоичного числа, половина — к чётному (форматирование Rust делает так же).
/// `(x * 10).round() / 10` расходится с пилотом на точных половинах.
pub fn pyround(x: f64, nd: usize) -> f64 {
    format!("{x:.nd$}").parse().unwrap_or(x)
}

// ---------------------------------------------------------------- события

/// Событие вызова в методе. Позиция — индекс в списке событий метода.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// `Модуль.Метод`, `*.Метод`, `param:Имя.Метод`, `Метод`, `Новый Тип`.
    pub name: String,
    /// Флаги блоков (`B` ветка, `C` условие, `L` цикл, `T` тело `Попытка`,
    /// `X` исключение) по алфавиту, плюс `:id` ближайшей `Попытка`.
    pub ctx: String,
    /// Вид первого аргумента: `S`, `N`, `I`, `E`, `-`, `=литерал`.
    pub ak: String,
    /// Описания первых четырёх аргументов.
    pub args: Vec<String>,
    /// Вызов — операнд `+`.
    pub concat: bool,
    /// Имя вызова, аргументом которого стоит этот, или пусто.
    pub parent: String,
}

impl Event {
    /// Символ события: имя, а при литеральном первом аргументе — с ним
    /// (`Режим(истина)` и `Режим(ложь)` — две стороны скобки).
    pub fn sym(&self) -> String {
        match self.ak.strip_prefix('=') {
            Some(l) => format!("{}({l})", self.name),
            None => self.name.clone(),
        }
    }

    fn flags(&self) -> &str {
        self.ctx.split(':').next().unwrap_or("")
    }

    fn try_id(&self) -> Option<&str> {
        self.ctx.split_once(':').map(|x| x.1)
    }
}

fn arg_kind(t: &[Tok], i: usize) -> String {
    let n = t.len();
    if i + 1 >= n {
        return "-".into();
    }
    let a = &t[i + 1];
    if a.text == ")" {
        return "-".into();
    }
    match a.kind {
        Kind::Str => {
            let mut j = i + 2;
            while j < n && t[j].kind == Kind::Str {
                j += 1;
            }
            let nxt = if j < n { t[j].text.as_str() } else { ")" };
            if nxt == ")" || nxt == "," { "S" } else { "E" }.into()
        }
        Kind::Num | Kind::Date => "N".into(),
        Kind::Id => {
            let nxt = if i + 2 < n {
                t[i + 2].text.as_str()
            } else {
                ")"
            };
            let closes = nxt == ")" || nxt == ",";
            if a.kw() == Some("LIT") {
                if closes {
                    format!("={}", a.low)
                } else {
                    "E".into()
                }
            } else if closes {
                "I".into()
            } else {
                "E".into()
            }
        }
        _ => "E".into(),
    }
}

struct LitRe {
    letters: Regex,
    digits: Regex,
    ws: Regex,
    quoted: Regex,
    ident: Regex,
    aaa: Regex,
    keys: Regex,
}

fn lit_re() -> &'static LitRe {
    static R: OnceLock<LitRe> = OnceLock::new();
    R.get_or_init(|| {
        let r = |p: &str| Regex::new(p).expect("регулярное выражение");
        LitRe {
            letters: r(r"[A-Za-zА-Яа-яЁё]+"),
            digits: r(r"\d+"),
            ws: r(r"\s+"),
            quoted: r(r"'[^']*'?"),
            ident: r(r"[A-Za-zА-Яа-яЁё_][\wЁё]*"),
            aaa: r(r"a(\s*a)+"),
            keys: r(r"([A-Za-zА-Яа-яЁё]+)\s*="),
        }
    })
}

/// Форма строкового литерала: грубая (`S:…`, для C4), точная (буквы → `a`,
/// цифры → `9`) и ключи вида `Ключ=` внутри строки (форматные строки, НСтр).
pub fn lit_desc(raw: &str) -> String {
    let re = lit_re();
    let body = raw.trim_matches('"').replace("\"\"", "\"");
    let shape = re.letters.replace_all(&body, "a");
    let shape = re.digits.replace_all(&shape, "9");
    let shape = re.ws.replace_all(&shape, " ");
    let shape: String = shape.chars().take(40).collect();
    let coarse = re.quoted.replace_all(&body, "'…'");
    let coarse = re.ident.replace_all(&coarse, "a");
    let coarse = re.digits.replace_all(&coarse, "9");
    let coarse = re.ws.replace_all(&coarse, "");
    let coarse = re.aaa.replace_all(&coarse, "a");
    let coarse: String = coarse.chars().take(24).collect();
    let keys: BTreeSet<&str> = re
        .keys
        .captures_iter(&body)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect();
    let keys: Vec<&str> = keys.into_iter().take(6).collect();
    let mut out = format!("S:{coarse}~{shape}");
    if !keys.is_empty() {
        out.push_str("|k=");
        out.push_str(&keys.join(","));
    }
    out
}

fn chain_like(ts: &[&Tok]) -> bool {
    ts.iter().step_by(2).all(|t| t.kind == Kind::Id)
        && ts.iter().skip(1).step_by(2).all(|t| t.text == ".")
}

/// Аргументы вызова, `(` в позиции `i`: описания первых четырёх и индекс `)`.
fn call_args(t: &[Tok], i: usize) -> (Vec<String>, usize) {
    let n = t.len();
    let mut depth = 0i64;
    let mut j = i;
    let mut args: Vec<Vec<&Tok>> = Vec::new();
    let mut cur: Vec<&Tok> = Vec::new();
    while j < n {
        let v = t[j].text.as_str();
        if v == "(" || v == "[" {
            depth += 1;
            if depth > 1 {
                cur.push(&t[j]);
            }
        } else if v == ")" || v == "]" {
            depth -= 1;
            if depth == 0 {
                args.push(std::mem::take(&mut cur));
                break;
            }
            cur.push(&t[j]);
        } else if v == "," && depth == 1 {
            args.push(std::mem::take(&mut cur));
        } else if depth >= 1 {
            cur.push(&t[j]);
        }
        j += 1;
    }
    let mut out = Vec::new();
    for a in args.iter().take(4) {
        let d = if a.is_empty() {
            "-".to_string()
        } else if a.iter().all(|t| t.kind == Kind::Str) {
            lit_desc(&a.iter().map(|t| t.text.as_str()).collect::<String>())
        } else if a.len() == 1 && matches!(a[0].kind, Kind::Num | Kind::Date) {
            "N".into()
        } else if a.len() == 1 && a[0].kind == Kind::Id {
            if a[0].kw() == Some("LIT") {
                format!("={}", a[0].low)
            } else {
                format!("I:{}", a[0].low)
            }
        } else if a.len() % 2 == 1 && chain_like(a) {
            format!("I:{}", a[a.len() - 1].text.to_lowercase())
        } else if a[0].kind == Kind::Id
            && a[a.len() - 1].text == ")"
            && a.iter().any(|t| t.text == "(")
        {
            // аргумент — один вызов f(...): имя функции до первой скобки
            let k = a.iter().position(|t| t.text == "(").unwrap_or(0);
            let head = &a[..k];
            if chain_like(head) {
                let mut depth = 0i64;
                let mut whole = true;
                for (q, t) in a.iter().enumerate().skip(k) {
                    depth += (t.text == "(") as i64;
                    depth -= (t.text == ")") as i64;
                    if depth == 0 && q != a.len() - 1 {
                        whole = false;
                        break;
                    }
                }
                if whole {
                    format!("C:{}", head[head.len() - 1].text)
                } else {
                    "E".into()
                }
            } else {
                "E".into()
            }
        } else {
            "E".into()
        };
        out.push(d);
    }
    if out.len() == 1 && out[0] == "-" {
        out.clear();
    }
    (out, j)
}

/// Вызов — операнд `+`: знак сразу перед началом цепочки или сразу после `)`.
fn concat(t: &[Tok], start: usize, end: usize) -> bool {
    let before = if start > 0 {
        t[start - 1].text.as_str()
    } else {
        ""
    };
    let after = if end + 1 < t.len() {
        t[end + 1].text.as_str()
    } else {
        ""
    };
    before == "+" || after == "+"
}

#[derive(Debug, Clone, Copy)]
enum Fr {
    T(u32),
    X(u32),
    L,
    C,
    B,
}

impl Fr {
    fn letter(self) -> char {
        match self {
            Fr::T(_) => 'T',
            Fr::X(_) => 'X',
            Fr::L => 'L',
            Fr::C => 'C',
            Fr::B => 'B',
        }
    }
}

fn ctx(st: &[Fr]) -> String {
    let mut f: Vec<char> = st.iter().map(|s| s.letter()).collect();
    f.sort_unstable();
    f.dedup();
    let mut s: String = f.into_iter().collect();
    if let Some(id) = st.iter().rev().find_map(|s| match s {
        Fr::T(i) | Fr::X(i) => Some(*i),
        _ => None,
    }) {
        s.push(':');
        s.push_str(&id.to_string());
    }
    s
}

/// Разобрать тело метода на события вызова. `resolve` нормализует цепочку
/// `a.b.c` перед `(` в имя вызова.
pub fn parse_method(body: &str, resolve: &dyn Fn(&[&str]) -> String) -> Vec<Event> {
    let t = tokens(&strip_comments(body));
    let n = t.len();
    let mut ev: Vec<Event> = Vec::new();
    let mut open: Vec<(usize, String)> = Vec::new();
    let mut st: Vec<Fr> = Vec::new();
    let mut try_id = 0u32;
    let mut i = 0usize;
    let pop_open = |open: &mut Vec<(usize, String)>, i: usize| {
        while open.last().is_some_and(|o| o.0 < i) {
            open.pop();
        }
    };
    while i < n {
        let tk = &t[i];
        let kw = if tk.kind == Kind::Id { tk.kw() } else { None };
        match kw {
            Some("TRY") => {
                try_id += 1;
                st.push(Fr::T(try_id));
            }
            Some("EXCEPT") => {
                if let Some(j) = st.iter().rposition(|s| matches!(s, Fr::T(_))) {
                    let Fr::T(tid) = st[j] else { unreachable!() };
                    st.truncate(j);
                    st.push(Fr::X(tid));
                }
            }
            Some("ENDTRY") => {
                if let Some(j) = st.iter().rposition(|s| matches!(s, Fr::T(_) | Fr::X(_))) {
                    st.truncate(j);
                }
            }
            Some(k @ ("IF" | "ELSIF")) => {
                if k == "ELSIF" && matches!(st.last(), Some(Fr::B)) {
                    st.pop();
                }
                st.push(Fr::C);
            }
            Some("THEN") => {
                if matches!(st.last(), Some(Fr::C)) {
                    st.pop();
                }
                st.push(Fr::B);
            }
            Some("ENDIF") => {
                if let Some(j) = st.iter().rposition(|s| matches!(s, Fr::B | Fr::C)) {
                    st.truncate(j);
                }
            }
            Some("WHILE") => st.push(Fr::C),
            Some("DO") => {
                if matches!(st.last(), Some(Fr::C)) {
                    st.pop();
                }
                st.push(Fr::L);
            }
            Some("ENDDO") => {
                if let Some(j) = st.iter().rposition(|s| matches!(s, Fr::L)) {
                    st.truncate(j);
                }
            }
            Some("NEW") => {
                if i + 1 < n && t[i + 1].kind == Kind::Id {
                    let name = format!("Новый {}", t[i + 1].text);
                    let (ak, args, end) = if i + 2 < n && t[i + 2].text == "(" {
                        let ak = arg_kind(&t, i + 2);
                        let (a, e) = call_args(&t, i + 2);
                        (ak, a, e)
                    } else {
                        ("-".to_string(), Vec::new(), i + 1)
                    };
                    pop_open(&mut open, i);
                    ev.push(Event {
                        name: name.clone(),
                        ctx: ctx(&st),
                        ak,
                        args,
                        concat: concat(&t, i, end),
                        parent: open.last().map(|o| o.1.clone()).unwrap_or_default(),
                    });
                    if end > i + 2 {
                        open.push((end, name));
                    }
                    i += 2;
                    continue;
                }
            }
            Some("RAISE") => {
                // Пилот передаёт позицию самого оператора: при `(` за ним
                // вид всегда «E». Сохранено ради сверки.
                let ak = if i + 1 < n && t[i + 1].text == "(" {
                    arg_kind(&t, i)
                } else {
                    "-".into()
                };
                ev.push(Event {
                    name: "ВызватьИсключение".into(),
                    ctx: ctx(&st),
                    ak,
                    args: Vec::new(),
                    concat: false,
                    parent: String::new(),
                });
            }
            None if tk.kind == Kind::Id => {
                let mut chain: Vec<&str> = vec![tk.text.as_str()];
                let mut j = i + 1;
                while j + 1 < n && t[j].text == "." && t[j + 1].kind == Kind::Id {
                    chain.push(&t[j + 1].text);
                    j += 2;
                }
                if j < n && t[j].text == "(" {
                    let prev = if i > 0 { t[i - 1].text.as_str() } else { "" };
                    // после '.' сюда попадает только вызов на результате
                    // выражения (x[0].Метод(), Ф().Метод()): начало — не имя
                    let name = if prev != "." {
                        resolve(&chain)
                    } else {
                        format!("*.{}", chain[chain.len() - 1])
                    };
                    let (args, end) = call_args(&t, j);
                    pop_open(&mut open, i);
                    ev.push(Event {
                        name: name.clone(),
                        ctx: ctx(&st),
                        ak: arg_kind(&t, j),
                        args,
                        concat: concat(&t, i, end),
                        parent: open.last().map(|o| o.1.clone()).unwrap_or_default(),
                    });
                    open.push((end, name));
                }
                i = j;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    ev
}

// ---------------------------------------------------------------- корпус

const GLOBAL_OBJ: &str = "Метаданные Справочники Документы РегистрыСведений РегистрыНакопления \
РегистрыБухгалтерии РегистрыРасчета Перечисления ПланыВидовХарактеристик ПланыСчетов \
ПланыВидовРасчета БизнесПроцессы Задачи ПланыОбмена Константы Обработки Отчеты ЖурналыДокументов \
ХранилищеОбщихНастроек ХранилищеСистемныхНастроек ПараметрыСеанса РегламентныеЗадания \
ФоновыеЗадания ПользователиИнформационнойБазы БлокировкаДанных ВнешниеОбработки ВнешниеОтчеты \
ЭтотОбъект ЭтаФорма Элементы Движения Параметры Объект Запись Форма";

/// Модуль корпуса.
pub struct Module {
    pub rel: String,
    pub category: Option<String>,
    pub object: Option<String>,
    pub module_type: Option<String>,
    pub is_form: bool,
}

/// Метод корпуса с событиями.
pub struct Meth {
    pub id: i64,
    pub module: i64,
    pub name: String,
    pub export: bool,
    pub directive: String,
    pub ev: Vec<Event>,
}

/// Модули по id и методы в порядке пилота.
pub struct Corpus {
    pub mods: HashMap<i64, Module>,
    pub meths: Vec<Meth>,
    /// Модули индекса, не прочитанные с диска (нет файла, не UTF-8).
    pub read_errors: usize,
}

fn opt(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("None")
}

fn resolve_chain(
    chain: &[&str],
    params: &HashSet<String>,
    common: &HashMap<String, String>,
    gobj: &HashSet<String>,
) -> String {
    if chain.len() == 1 {
        return chain[0].to_string();
    }
    let last = chain[chain.len() - 1];
    let head = chain[0].to_lowercase();
    if params.contains(&head) {
        return format!("param:{}.{last}", chain[0]);
    }
    if let Some(c) = common.get(&head) {
        return format!("{c}.{last}");
    }
    if gobj.contains(&head) {
        let mid = if chain.len() > 2 { ".*" } else { "" };
        return format!("{}{mid}.{last}", chain[0]);
    }
    format!("*.{last}")
}

/// Прочитать методы конфигурации и разобрать их на события вызова.
pub fn corpus(conn: &Connection, source: &Path) -> Result<Corpus, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let mut common: HashMap<String, String> = HashMap::new();
    let mut st = conn
        .prepare("SELECT DISTINCT object_name FROM modules WHERE category = 'CommonModules'")
        .map_err(e)?;
    for r in st
        .query_map([], |r| r.get::<_, Option<String>>(0))
        .map_err(e)?
    {
        if let Some(o) = r.map_err(e)? {
            common.insert(o.to_lowercase(), o);
        }
    }
    let gobj: HashSet<String> = GLOBAL_OBJ
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();

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
    type Row = (i64, String, bool, Option<String>, Option<i64>, Option<i64>);
    let mut by_module: HashMap<i64, Vec<Row>> = HashMap::new();
    let mut st = conn
        .prepare("SELECT id, module_id, name, is_export, params, line, end_line FROM methods")
        .map_err(e)?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(1)?,
                (
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                ),
            ))
        })
        .map_err(e)?;
    for r in rows {
        let (m, rec) = r.map_err(e)?;
        by_module.entry(m).or_default().push(rec);
    }

    let parsed: Vec<Option<(Module, Vec<Meth>)>> = mods
        .par_iter()
        .map(|(id, rel, cat, obj, mt, is_form)| {
            let bytes = std::fs::read(source.join(rel)).ok()?;
            let text = String::from_utf8(bytes).ok()?;
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            let text = newlines(text);
            let lines: Vec<&str> = text.split('\n').collect();
            let mut recs = Vec::new();
            for (mid, name, export, params, line, end) in by_module.get(id).into_iter().flatten() {
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
                let ps = param_names(params.as_deref());
                let resolve = |c: &[&str]| resolve_chain(c, &ps, &common, &gobj);
                recs.push(Meth {
                    id: *mid,
                    module: *id,
                    name: name.clone(),
                    export: *export,
                    directive,
                    ev: parse_method(&body, &resolve),
                });
            }
            Some((
                Module {
                    rel: rel.clone(),
                    category: cat.clone(),
                    object: obj.clone(),
                    module_type: mt.clone(),
                    is_form: *is_form,
                },
                recs,
            ))
        })
        .collect();
    let mut out = Corpus {
        mods: HashMap::new(),
        meths: Vec::new(),
        read_errors: 0,
    };
    for ((id, ..), r) in mods.iter().zip(parsed) {
        if let Some((m, recs)) = r {
            out.mods.insert(*id, m);
            out.meths.extend(recs);
        } else {
            out.read_errors += 1;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- счётчики

/// Счётчик, помнящий порядок первой встречи ключа: «самые частые» при
/// равенстве — как `Counter.most_common` пилота.
#[derive(Default, Clone)]
struct OrdCounter {
    idx: HashMap<String, usize>,
    items: Vec<(String, u64)>,
}

impl OrdCounter {
    fn add_n(&mut self, k: &str, n: u64) {
        match self.idx.get(k) {
            Some(&i) => self.items[i].1 += n,
            None => {
                self.idx.insert(k.to_string(), self.items.len());
                self.items.push((k.to_string(), n));
            }
        }
    }
    fn add(&mut self, k: &str) {
        self.add_n(k, 1);
    }
    fn get(&self, k: &str) -> u64 {
        self.idx.get(k).map_or(0, |&i| self.items[i].1)
    }
    fn total(&self) -> u64 {
        self.items.iter().map(|x| x.1).sum()
    }
    fn most_common(&self, n: usize) -> Vec<(&str, u64)> {
        let mut v: Vec<(usize, &(String, u64))> = self.items.iter().enumerate().collect();
        v.sort_by(|a, b| b.1 .1.cmp(&a.1 .1).then(a.0.cmp(&b.0)));
        v.into_iter()
            .take(n)
            .map(|(_, x)| (x.0.as_str(), x.1))
            .collect()
    }
}

/// Словарь символов: строка ↔ номер.
#[derive(Default)]
struct Interner {
    ids: HashMap<String, u32>,
    names: Vec<String>,
}

impl Interner {
    fn id(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.ids.get(s) {
            return i;
        }
        let i = self.names.len() as u32;
        self.ids.insert(s.to_string(), i);
        self.names.push(s.to_string());
        i
    }
    fn get(&self, s: &str) -> Option<u32> {
        self.ids.get(s).copied()
    }
    fn name(&self, i: u32) -> &str {
        &self.names[i as usize]
    }
}

/// Символы корпуса: у каждого метода последовательность символов событий,
/// их множество и множество с признаком серверного вызова из формы.
struct Syms {
    it: Interner,
    seq: Vec<Vec<u32>>,
    set: Vec<Vec<u32>>,
    mset: Vec<Vec<u32>>,
}

const SERVER_CALL: &str = "@серверный-вызов";

fn syms(c: &Corpus) -> Syms {
    let mut it = Interner::default();
    let server = it.id(SERVER_CALL);
    // серверный вызов: локальный метод того же модуля с директивой &НаСервере…
    let mut mdir: HashMap<(i64, String), &str> = HashMap::new();
    for m in &c.meths {
        mdir.insert((m.module, m.name.to_lowercase()), &m.directive);
    }
    let mut seq = Vec::with_capacity(c.meths.len());
    let mut set = Vec::with_capacity(c.meths.len());
    let mut mset = Vec::with_capacity(c.meths.len());
    for m in &c.meths {
        let s: Vec<u32> = m.ev.iter().map(|e| it.id(&e.sym())).collect();
        let mut u = s.clone();
        u.sort_unstable();
        u.dedup();
        let mut mu = u.clone();
        if c.mods[&m.module].is_form
            && m.ev.iter().any(|e| {
                !e.name.contains('.')
                    && mdir
                        .get(&(m.module, e.name.to_lowercase()))
                        .is_some_and(|d| d.starts_with("&НаСервере"))
            })
            && !mu.contains(&server)
        {
            mu.push(server);
            mu.sort_unstable();
        }
        seq.push(s);
        set.push(u);
        mset.push(mu);
    }
    Syms { it, seq, set, mset }
}

// ---------------------------------------------------------------- C1

const C1_RELS: [&str; 7] = [
    "A_first",
    "B_first",
    "B_after_last_A",
    "B_only_in_except",
    "B_only_in_try_body",
    "B_in_except_of_A_try",
    "both_in_loop",
];

/// C1 — «если в методе есть A, то в нём же есть B» с отношением пары.
fn mine_c1(c: &Corpus, s: &Syms) -> Vec<Value> {
    const MIN_SUP: u64 = 20;
    const MIN_CONF: f64 = 0.90;
    const MIN_LIFT: f64 = 3.0;
    const REL: f64 = 0.90;
    let n = c.meths.len();
    let mut df: HashMap<u32, u64> = HashMap::new();
    for ss in &s.set {
        for &x in ss {
            *df.entry(x).or_default() += 1;
        }
    }
    let frequent: HashSet<u32> = df
        .iter()
        .filter(|(_, &v)| v >= MIN_SUP)
        .map(|(&k, _)| k)
        .collect();
    let co: HashMap<(u32, u32), u64> = s
        .set
        .par_iter()
        .fold(HashMap::new, |mut co: HashMap<(u32, u32), u64>, ss| {
            for &a in ss.iter().filter(|a| frequent.contains(a)) {
                for &b in ss {
                    if a != b {
                        *co.entry((a, b)).or_default() += 1;
                    }
                }
            }
            co
        })
        .reduce(HashMap::new, |mut x, y| {
            for (k, v) in y {
                *x.entry(k).or_default() += v;
            }
            x
        });
    let mut cand: Vec<(u32, u32, u64, f64, f64)> = Vec::new();
    for (&(a, b), &cnt) in &co {
        let conf = cnt as f64 / df[&a] as f64;
        if conf < MIN_CONF {
            continue;
        }
        let lift = conf / (df[&b] as f64 / n as f64);
        if lift < MIN_LIFT {
            continue;
        }
        cand.push((a, b, cnt, conf, lift));
    }
    let mut by_a: HashMap<u32, HashSet<u32>> = HashMap::new();
    for &(a, b, ..) in &cand {
        by_a.entry(a).or_default().insert(b);
    }
    // отношения по методам пары
    let mut rel: HashMap<(u32, u32), [u64; 8]> = HashMap::new();
    for (mi, m) in c.meths.iter().enumerate() {
        let ss = &s.set[mi];
        let seq = &s.seq[mi];
        let mut pos: HashMap<u32, Vec<usize>> = HashMap::new();
        for (k, &x) in seq.iter().enumerate() {
            pos.entry(x).or_default().push(k);
        }
        for &a in ss {
            let Some(bs) = by_a.get(&a) else { continue };
            for &b in ss {
                if !bs.contains(&b) {
                    continue;
                }
                let r = rel.entry((a, b)).or_default();
                r[7] += 1;
                let ea = &pos[&a];
                let eb = &pos[&b];
                let ev = |k: usize| &m.ev[k];
                if ea[0] < eb[0] {
                    r[0] += 1;
                }
                if ea[0] > eb[0] {
                    r[1] += 1;
                }
                if eb[eb.len() - 1] > ea[ea.len() - 1] {
                    r[2] += 1;
                }
                if eb.iter().all(|&k| ev(k).flags().contains('X')) {
                    r[3] += 1;
                }
                if eb.iter().all(|&k| ev(k).flags().contains('T')) {
                    r[4] += 1;
                }
                let ta: HashSet<&str> = ea
                    .iter()
                    .filter(|&&k| ev(k).flags().contains('T'))
                    .filter_map(|&k| ev(k).try_id())
                    .collect();
                if eb
                    .iter()
                    .filter(|&&k| ev(k).flags().contains('X'))
                    .filter_map(|&k| ev(k).try_id())
                    .any(|x| ta.contains(x))
                {
                    r[5] += 1;
                }
                if ea.iter().all(|&k| ev(k).flags().contains('L'))
                    && eb.iter().all(|&k| ev(k).flags().contains('L'))
                {
                    r[6] += 1;
                }
            }
        }
    }
    cand.sort_by(|x, y| {
        let kx = -(x.3 * x.2.min(500) as f64);
        let ky = -(y.3 * y.2.min(500) as f64);
        kx.total_cmp(&ky)
            .then_with(|| s.it.name(x.0).cmp(s.it.name(y.0)))
            .then_with(|| s.it.name(x.1).cmp(s.it.name(y.1)))
    });
    cand.iter()
        .map(|&(a, b, cnt, conf, lift)| {
            let r = rel.get(&(a, b)).copied().unwrap_or_default();
            let nn = if r[7] == 0 { 1 } else { r[7] } as f64;
            let rels: Vec<&str> = C1_RELS
                .iter()
                .enumerate()
                .filter(|(i, _)| r[*i] as f64 / nn >= REL)
                .map(|(_, k)| *k)
                .collect();
            json!({"A": s.it.name(a), "B": s.it.name(b), "support": cnt, "nA": df[&a],
                   "conf": pyround(conf, 3), "lift": pyround(lift, 1), "relations": rels})
        })
        .collect()
}

// ---------------------------------------------------------------- C2–C6

fn root(a: &str) -> Option<&str> {
    (a.contains('.') && !a.starts_with("*.")).then(|| a.split('.').next().unwrap_or(""))
}

fn base(s: &str) -> &str {
    s.split('(').next().unwrap_or("")
}

fn words(name: &str) -> Vec<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    let re = R.get_or_init(|| Regex::new(r"[А-ЯA-ZЁ][а-яa-zё]+").expect("регулярное выражение"));
    let last = name.rsplit('.').next().unwrap_or("").replace("Новый ", "");
    re.find_iter(&last)
        .map(|m| m.as_str().to_lowercase())
        .collect()
}

fn prefix5(w: &str) -> String {
    w.chars().take(5).collect()
}

/// Сведения из индекса и справки, которые нужны C2–C5.
pub struct Env {
    pub scheduled: HashSet<(String, String)>,
    pub subscriptions: HashSet<(String, String)>,
    pub platform: HashSet<String>,
    pub global_methods: HashSet<String>,
    /// (callee_key, caller_id) из графа вызовов.
    pub calls: Vec<(String, i64)>,
    pub form_handlers: HashSet<String>,
}

const GOBJ_C3: [&str; 15] = [
    "Метаданные",
    "Справочники",
    "Документы",
    "РегистрыСведений",
    "РегистрыНакопления",
    "ХранилищеОбщихНастроек",
    "ХранилищеСистемныхНастроек",
    "РегламентныеЗадания",
    "ФоновыеЗадания",
    "ПользователиИнформационнойБазы",
    "ПараметрыСеанса",
    "ВнешниеОбработки",
    "ВнешниеОтчеты",
    "Константы",
    "ПланыОбмена",
];

/// Прочитать из индекса всё, что нужно механизмам C2–C5.
pub fn env(conn: &Connection, global_methods: HashSet<String>) -> Result<Env, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let pairs = |sql: &str| -> Result<HashSet<(String, String)>, String> {
        let mut st = conn.prepare(sql).map_err(e)?;
        let v = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })
            .map_err(e)?
            .collect::<Result<_, _>>()
            .map_err(e)?;
        Ok(v)
    };
    let names = |sql: &str| -> Result<Vec<String>, String> {
        let mut st = conn.prepare(sql).map_err(e)?;
        let v = st
            .query_map([], |r| r.get::<_, Option<String>>(0))
            .map_err(e)?
            .filter_map(|x| x.ok().flatten())
            .collect();
        Ok(v)
    };
    let mut platform: HashSet<String> =
        names("SELECT DISTINCT callee_name FROM calls WHERE resolution = 'platform_global'")?
            .into_iter()
            .collect();
    for n in names("SELECT DISTINCT callee_name FROM calls WHERE resolution = 'platform_var'")? {
        if GOBJ_C3.contains(&n.split('.').next().unwrap_or("")) && n.matches('.').count() == 1 {
            platform.insert(n);
        }
    }
    let mut st = conn
        .prepare("SELECT callee_key, caller_id FROM calls WHERE callee_key IS NOT NULL")
        .map_err(e)?;
    let calls = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(e)?
        .collect::<Result<_, _>>()
        .map_err(e)?;
    let has_handler = conn
        .prepare("SELECT 1 FROM pragma_table_info('form_elements') WHERE name = 'handler'")
        .and_then(|mut s| s.exists([]))
        .unwrap_or(false);
    let form_handlers = if has_handler {
        names("SELECT DISTINCT handler FROM form_elements WHERE handler IS NOT NULL")?
            .into_iter()
            .collect()
    } else {
        HashSet::new()
    };
    Ok(Env {
        scheduled: pairs("SELECT handler_module, handler_procedure FROM scheduled_jobs")?,
        subscriptions: pairs("SELECT handler_module, handler_procedure FROM event_subscriptions")?,
        platform,
        global_methods,
        calls,
        form_handlers,
    })
}

fn names_in_50(c: &Corpus) -> HashSet<String> {
    let mut nm: HashMap<&str, HashSet<i64>> = HashMap::new();
    for m in &c.meths {
        nm.entry(&m.name).or_default().insert(m.module);
    }
    nm.into_iter()
        .filter(|(_, s)| s.len() >= 50)
        .map(|(n, _)| n.to_string())
        .collect()
}

fn mt_key(md: &Module) -> String {
    format!(
        "mt:{}/{}{}",
        opt(&md.category),
        opt(&md.module_type),
        if md.is_form { "/form" } else { "" }
    )
}

/// C2–C6 по корпусу, графу и выдаче C1.
fn mine_c2_c6(c: &Corpus, s: &Syms, c1: &[Value], env: &Env) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let nmeth = c.meths.len();
    let handlers = names_in_50(c);

    // ---------- C2 ----------
    let contexts = |m: &Meth| -> (String, Vec<String>) {
        let md = &c.mods[&m.module];
        let dr = match m.directive.split('(').next().unwrap_or("") {
            "" => "&<нет>".to_string(),
            d => d.to_string(),
        };
        let mut ks = vec![format!("dir:{dr}"), mt_key(md)];
        if handlers.contains(&m.name) {
            ks.push(format!(
                "h:{}{}",
                m.name,
                if md.is_form { "/form" } else { "" }
            ));
        }
        let cm = opt(&md.category) == "CommonModules";
        let key = (opt(&md.object).to_string(), m.name.clone());
        if cm && env.scheduled.contains(&key) {
            ks.push("sj".into());
        }
        if cm && env.subscriptions.contains(&key) {
            ks.push("es".into());
        }
        (dr, ks)
    };
    let ctxs: Vec<(String, Vec<String>)> = c.meths.iter().map(contexts).collect();

    let mut n_k = OrdCounter::default();
    let mut n_dir = OrdCounter::default();
    let mut kdir: HashMap<String, OrdCounter> = HashMap::new();
    let mut o_ak: HashMap<(u32, String), u64> = HashMap::new();
    let mut first_ak: HashMap<(u32, String), u64> = HashMap::new();
    let mut n_adir: HashMap<(u32, String), u64> = HashMap::new();
    let mut n_a: HashMap<u32, u64> = HashMap::new();
    let mut n_mt: HashMap<String, u64> = HashMap::new();
    let mut n_amt: HashMap<(u32, String), u64> = HashMap::new();
    let mut kmt: HashMap<String, OrdCounter> = HashMap::new();
    for (mi, (dr, ks)) in ctxs.iter().enumerate() {
        let mt = &ks[1];
        *n_mt.entry(mt.clone()).or_default() += 1;
        for k in ks {
            kmt.entry(k.clone()).or_default().add(mt);
        }
        let ss = &s.mset[mi];
        for &a in ss {
            *n_amt.entry((a, mt.clone())).or_default() += 1;
        }
        n_dir.add(dr);
        for k in ks {
            n_k.add(k);
            kdir.entry(k.clone()).or_default().add(dr);
        }
        for &a in ss {
            *n_a.entry(a).or_default() += 1;
            *n_adir.entry((a, dr.clone())).or_default() += 1;
            for k in ks {
                *o_ak.entry((a, k.clone())).or_default() += 1;
            }
        }
        if let Some(&f) = s.seq[mi].first() {
            for k in ks {
                *first_ak.entry((f, k.clone())).or_default() += 1;
            }
        }
    }
    let frequent: Vec<u32> = {
        let mut v: Vec<u32> = n_a
            .iter()
            .filter(|(_, &c)| c >= 30)
            .map(|(&a, _)| a)
            .collect();
        v.sort_unstable();
        v
    };
    let get = |m: &HashMap<(u32, String), u64>, a: u32, k: &str| {
        *m.get(&(a, k.to_string())).unwrap_or(&0)
    };
    let name = |a: u32| s.it.name(a);
    for (k, nk) in &n_k.items {
        let nk = *nk;
        let Some(&(dr, _)) = kdir[k].most_common(1).first() else {
            continue;
        };
        let nd = n_dir.get(dr) as f64;
        let mt = kmt[k]
            .most_common(1)
            .first()
            .map(|x| x.0.to_string())
            .unwrap_or_default();
        for &a in &frequent {
            let p = get(&n_adir, a, dr) as f64 / nd;
            let e = nk as f64 * p;
            let o = get(&o_ak, a, k);
            if avoid(o, e) && !k.starts_with("dir:") {
                out.push(
                    json!({"mech": "C2-avoid", "A": name(a), "K": k, "observed": o,
                    "expected": pyround(e, 1), "nK": nk}),
                );
            } else if k.starts_with("h:") {
                // вторая база: тот же вид модуля (а не та же директива)
                let e2 = nk as f64 * get(&n_amt, a, &mt) as f64 / n_mt[&mt] as f64;
                if avoid(o, e2) {
                    out.push(
                        json!({"mech": "C2-avoid-mt", "A": name(a), "K": k, "observed": o,
                        "expected": pyround(e2, 1), "nK": nk, "mt": mt}),
                    );
                }
            }
        }
    }
    // обязательность
    for ((a, k), &o) in &o_ak {
        let nk = n_k.get(k);
        if nk >= 20
            && o as f64 >= 0.85 * nk as f64
            && !k.starts_with("dir:")
            && (n_a[a] as f64) < 0.2 * nmeth as f64
        {
            let mut rec =
                json!({"mech": "C2-require", "A": name(*a), "K": k, "observed": o, "nK": nk});
            if get(&first_ak, *a, k) as f64 >= 0.85 * nk as f64 {
                rec["first"] = json!(true);
            }
            out.push(rec);
        }
    }
    // условное избегание: среди методов, работающих с корнем R символа A
    let mut root_k: HashMap<(String, String), u64> = HashMap::new();
    let mut root_n: HashMap<String, u64> = HashMap::new();
    let mut aroot: HashMap<u32, u64> = HashMap::new();
    let mut aroot_k: HashMap<(u32, String), u64> = HashMap::new();
    for (mi, _) in c.meths.iter().enumerate() {
        let ss = &s.mset[mi];
        let ks = &ctxs[mi].1;
        let rs: HashSet<&str> = ss.iter().filter_map(|&x| root(name(x))).collect();
        for r in rs {
            *root_n.entry(r.to_string()).or_default() += 1;
            for k in ks.iter().filter(|k| !k.starts_with("dir:")) {
                *root_k.entry((r.to_string(), k.clone())).or_default() += 1;
            }
        }
        for &a in ss {
            if root(name(a)).is_some() {
                *aroot.entry(a).or_default() += 1;
                for k in ks.iter().filter(|k| !k.starts_with("dir:")) {
                    *aroot_k.entry((a, k.clone())).or_default() += 1;
                }
            }
        }
    }
    let mut by_root: HashMap<&str, Vec<u32>> = HashMap::new();
    for (&a, &cnt) in &aroot {
        if cnt >= 30 {
            if let Some(r) = root(name(a)) {
                by_root.entry(r).or_default().push(a);
            }
        }
    }
    for ((r, k), &nrk) in &root_k {
        if nrk < 20 {
            continue;
        }
        for &a in by_root.get(r.as_str()).into_iter().flatten() {
            let o = get(&aroot_k, a, k);
            let rest = root_n[r] as i64 - nrk as i64;
            if rest <= 0 {
                continue;
            }
            let e = nrk as f64 * (aroot[&a] - o) as f64 / rest as f64;
            if avoid(o, e) {
                out.push(
                    json!({"mech": "C2-avoid-cond", "A": name(a), "K": k, "root": r,
                    "observed": o, "expected": pyround(e, 1), "nK_root": nrk}),
                );
            }
        }
    }

    // ---------- C2-block: контекст «внутри блока» ----------
    let mut brackets: BTreeSet<(String, String)> = BTreeSet::new();
    for r in c1 {
        if r["conf"].as_f64().unwrap_or(0.0) < 0.9 || r["support"].as_u64().unwrap_or(0) < 100 {
            continue;
        }
        let rels: Vec<&str> = r["relations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let (a, b) = (r["A"].as_str().unwrap_or(""), r["B"].as_str().unwrap_or(""));
        if rels.contains(&"A_first") && rels.contains(&"B_after_last_A") {
            brackets.insert((a.to_string(), b.to_string()));
        } else if rels.contains(&"B_first") {
            brackets.insert((b.to_string(), a.to_string()));
        }
    }
    let mut br_by_op: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
    for (op, cl) in &brackets {
        if let (Some(o), Some(cc)) = (s.it.get(op), s.it.get(cl)) {
            br_by_op
                .entry(o)
                .or_default()
                .push((cc, format!("[{op}..{cl}]")));
        }
    }
    let mut ins: HashMap<(String, u32), u64> = HashMap::new();
    let mut tot: HashMap<(String, u32), u64> = HashMap::new();
    let mut ev_in: HashMap<String, u64> = HashMap::new();
    let mut ev_all: HashMap<String, u64> = HashMap::new();
    for (mi, m) in c.meths.iter().enumerate() {
        let seq = &s.seq[mi];
        if seq.is_empty() {
            continue;
        }
        let mut spans: Vec<(String, Vec<bool>)> = ["T", "X", "L"]
            .iter()
            .map(|f| {
                (
                    f.to_string(),
                    m.ev.iter().map(|e| e.flags().contains(f)).collect(),
                )
            })
            .collect();
        let present: HashSet<u32> = s.set[mi].iter().copied().collect();
        for &op in &s.set[mi] {
            for (cl, key) in br_by_op.get(&op).into_iter().flatten() {
                if !present.contains(cl) {
                    continue;
                }
                let i = seq.iter().position(|&x| x == op).unwrap_or(0);
                let j = seq.iter().rposition(|x| x == cl).unwrap_or(0);
                if j > i {
                    spans.push((
                        key.clone(),
                        (0..seq.len()).map(|q| i < q && q < j).collect(),
                    ));
                }
            }
        }
        for (b, mask) in spans {
            if !mask.iter().any(|&x| x) {
                continue;
            }
            *ev_in.entry(b.clone()).or_default() += mask.iter().filter(|&&x| x).count() as u64;
            *ev_all.entry(b.clone()).or_default() += mask.len() as u64;
            for (&sy, &inside) in seq.iter().zip(&mask) {
                *tot.entry((b.clone(), sy)).or_default() += 1;
                *ins.entry((b.clone(), sy)).or_default() += inside as u64;
            }
        }
    }
    for ((b, sy), &t) in &tot {
        if t < 30 || !ev_all.contains_key(b) {
            continue;
        }
        let sname = name(*sy);
        if b.starts_with('[') && b[1..b.len() - 1].split("..").any(|x| x == sname) {
            continue;
        }
        let frac = ev_in[b] as f64 / ev_all[b] as f64;
        let o = ins[&(b.clone(), *sy)];
        let e = t as f64 * frac;
        if avoid(o, e) {
            out.push(
                json!({"mech": "C2-block-avoid", "A": sname, "K": b, "observed": o,
                "expected": pyround(e, 1), "n": t}),
            );
        } else if o as f64 >= 0.85 * t as f64 && frac < 0.5 && o as f64 / t as f64 >= 3.0 * frac {
            out.push(
                json!({"mech": "C2-block-require", "A": sname, "K": b, "observed": o, "n": t,
                "base": pyround(frac, 3)}),
            );
        }
    }
    // избегание по директиве
    for &a in &frequent {
        for (dr, nd) in &n_dir.items {
            let nd = *nd;
            if nd < 500 {
                continue;
            }
            let rest_n = nmeth as u64 - nd;
            let o = get(&n_adir, a, dr);
            let p_rest = if rest_n > 0 {
                (n_a[&a] - o) as f64 / rest_n as f64
            } else {
                0.0
            };
            let e = nd as f64 * p_rest;
            if avoid(o, e) && o as f64 <= 0.02 * e {
                out.push(
                    json!({"mech": "C2-avoid-dir", "A": name(a), "K": format!("dir:{dr}"),
                    "observed": o, "expected": pyround(e, 1)}),
                );
            }
        }
    }

    // ---------- C3 ----------
    let mut exports_common: HashMap<i64, String> = HashMap::new();
    for m in &c.meths {
        let md = &c.mods[&m.module];
        if opt(&md.category) == "CommonModules" && m.export {
            exports_common.insert(m.id, format!("{}.{}", opt(&md.object), m.name));
        }
    }
    let mut calls_cnt: HashMap<u32, u64> = HashMap::new();
    for ss in &s.mset {
        for &x in ss {
            *calls_cnt.entry(x).or_default() += 1;
        }
    }
    let cnt_of = |w: &str| {
        s.it.get(w)
            .and_then(|i| calls_cnt.get(&i))
            .copied()
            .unwrap_or(0)
    };
    let mut wrappers: HashMap<String, HashSet<String>> = HashMap::new();
    let mut export_bases: Vec<(&str, HashSet<&str>)> = Vec::new();
    for (mi, m) in c.meths.iter().enumerate() {
        if let Some(en) = exports_common.get(&m.id) {
            let bases: HashSet<&str> = s.mset[mi].iter().map(|&x| base(name(x))).collect();
            for b in &bases {
                if env.platform.contains(*b) {
                    wrappers
                        .entry(b.to_string())
                        .or_default()
                        .insert(en.clone());
                }
            }
            export_bases.push((en.as_str(), bases));
        }
    }
    let mut by_base: HashMap<&str, u64> = HashMap::new();
    for (&x, &cnt) in &calls_cnt {
        *by_base.entry(base(name(x))).or_default() += cnt;
    }
    let mut wl: Vec<(&String, &HashSet<String>)> = wrappers.iter().collect();
    wl.sort_by(|a, b| a.0.cmp(b.0));
    for (p, wnames) in wl {
        let inside = export_bases
            .iter()
            .filter(|(en, bases)| wnames.contains(*en) && bases.contains(p.as_str()))
            .count() as i64;
        let total_p = *by_base.get(p.as_str()).unwrap_or(&0) as i64;
        let out_p = total_p - inside;
        let in_w: i64 = wnames.iter().map(|w| cnt_of(w) as i64).sum();
        if in_w >= 30 && out_p as f64 <= 0.1 * (out_p + in_w) as f64 {
            let mut top: Vec<&String> = wnames.iter().collect();
            top.sort_by(|a, b| cnt_of(b).cmp(&cnt_of(a)).then(a.cmp(b)));
            let top: Vec<&String> = top.into_iter().take(3).collect();
            out.push(
                json!({"mech": "C3-wrapper", "P": p, "P_outside": out_p, "wrapper_calls": in_w,
                "wrappers": top}),
            );
        }
    }
    let mut stem: HashMap<String, HashSet<&str>> = HashMap::new();
    for &x in calls_cnt.keys() {
        let sx = name(x);
        for w in words(sx) {
            if w.chars().count() >= 5 {
                stem.entry(prefix5(&w)).or_default().insert(base(sx));
            }
        }
    }
    let mut ps: BTreeSet<String> = env.platform.iter().cloned().collect();
    ps.extend(env.global_methods.iter().cloned());
    ps.extend(
        calls_cnt
            .keys()
            .map(|&x| name(x))
            .filter(|x| x.starts_with("Новый "))
            .map(str::to_string),
    );
    for p in &ps {
        let cp = *by_base.get(p.as_str()).unwrap_or(&0);
        let wp = words(p);
        let mut cands: BTreeSet<&str> = BTreeSet::new();
        for w in &wp {
            if w.chars().count() >= 5 {
                cands.extend(stem.get(&prefix5(w)).into_iter().flatten().copied());
            }
        }
        let sp: HashSet<String> = wp.iter().map(|w| prefix5(w)).collect();
        for q in cands {
            if q == p {
                continue;
            }
            let cq = *by_base.get(q).unwrap_or(&0);
            let sq: HashSet<String> = words(q).iter().map(|w| prefix5(w)).collect();
            let uni = sp.union(&sq).count();
            if uni == 0 {
                continue;
            }
            let jac = sp.intersection(&sq).count() as f64 / uni as f64;
            if cq >= 100 && cq >= 10 * cp.max(1) && jac >= 0.5 {
                out.push(
                    json!({"mech": "C3-similar", "P": p, "P_calls": cp, "Q": q, "Q_calls": cq,
                    "jaccard": pyround(jac, 2)}),
                );
            }
        }
    }

    // ---------- C4 ---------- (все позиции; строка — грубой формой)
    let desc = |x: &str| -> String {
        if x.starts_with("C:") || x.starts_with("I:") || x.starts_with('=') {
            x.to_string()
        } else if x.starts_with("S:") {
            x.split('~').next().unwrap_or("").to_string()
        } else {
            x.chars().take(1).collect()
        }
    };
    let mut pos: HashMap<(String, usize), OrdCounter> = HashMap::new();
    let mut pos_order: Vec<(String, usize)> = Vec::new();
    for m in &c.meths {
        for e in &m.ev {
            for (k, x) in e.args.iter().enumerate() {
                let key = (e.name.clone(), k);
                let cnt = pos.entry(key.clone()).or_insert_with(|| {
                    pos_order.push(key);
                    OrdCounter::default()
                });
                cnt.add(&desc(x));
            }
        }
    }
    for key in &pos_order {
        let kc = &pos[key];
        let n = kc.total();
        if n < 30 {
            continue;
        }
        let (a, k) = key;
        let (v, cnt) = kc.most_common(1)[0];
        if !matches!(v, "-" | "N" | "E") && cnt as f64 >= 0.95 * n as f64 {
            out.push(json!({"mech": "C4-arg", "A": a, "pos": k + 1, "value": v,
                "share": pyround(cnt as f64 / n as f64, 3), "n": n}));
            continue;
        }
        // уровень вида: любая строка-литерал / любой идентификатор
        let mut kinds = OrdCounter::default();
        for (x, cx) in &kc.items {
            let two: String = x.chars().take(2).collect();
            let kk = if matches!(two.as_str(), "S:" | "I:" | "C:") {
                two
            } else {
                x.clone()
            };
            kinds.add_n(&kk, *cx);
        }
        let (v, cnt) = kinds.most_common(1)[0];
        let label = match v {
            "S:" => Some("строка-литерал"),
            "I:" => Some("идентификатор"),
            "C:" => Some("вызов функции"),
            _ => None,
        };
        if let Some(label) = label {
            if cnt as f64 >= 0.95 * n as f64 {
                out.push(
                    json!({"mech": "C4-arg", "A": a, "pos": k + 1, "value": label,
                    "share": pyround(cnt as f64 / n as f64, 3), "n": n}),
                );
            }
        }
    }

    // ---------- C4-nokey: ключ форматной строки, которого у A нет ----------
    let mut lits: Vec<(&str, usize, BTreeSet<&str>)> = Vec::new();
    for m in &c.meths {
        for e in &m.ev {
            for (k, x) in e.args.iter().enumerate() {
                if x.starts_with("S:") {
                    if let Some((_, ks)) = x.split_once("|k=") {
                        lits.push((&e.name, k, ks.split(',').collect()));
                    }
                }
            }
        }
    }
    let mut by_fn: HashMap<(&str, usize), Vec<&BTreeSet<&str>>> = HashMap::new();
    for (f, k, ks) in &lits {
        by_fn.entry((f, *k)).or_default().push(ks);
    }
    for ((a, k), own) in &by_fn {
        let na = own.len();
        if na < 100 {
            continue;
        }
        let mut kc: HashMap<&str, u64> = HashMap::new();
        for ks in own {
            for &kk in ks.iter() {
                *kc.entry(kk).or_default() += 1;
            }
        }
        let vocab: HashSet<&str> = kc
            .iter()
            .filter(|(_, &c)| c as f64 >= 0.01 * na as f64)
            .map(|(&k, _)| k)
            .collect();
        let rel: Vec<&BTreeSet<&str>> = lits
            .iter()
            .filter(|(f, kk, ks)| (f, kk) != (a, k) && ks.iter().any(|x| vocab.contains(x)))
            .map(|(_, _, ks)| ks)
            .collect();
        if rel.len() < 100 {
            continue;
        }
        let mut rc: HashMap<&str, u64> = HashMap::new();
        for ks in &rel {
            for &kk in ks.iter() {
                *rc.entry(kk).or_default() += 1;
            }
        }
        for (kk, &cnt) in &rc {
            if cnt < 30 || vocab.contains(kk) {
                continue;
            }
            let o = *kc.get(kk).unwrap_or(&0);
            let e = na as f64 * cnt as f64 / rel.len() as f64;
            if avoid(o, e) {
                out.push(
                    json!({"mech": "C4-nokey", "A": a, "pos": k + 1, "key": kk, "observed": o,
                    "expected": pyround(e, 1), "n": na, "related": rel.len()}),
                );
            }
        }
    }

    // ---------- C6 ---------- склейка и вложенность
    let mut nn: HashMap<&str, u64> = HashMap::new();
    let mut cc: HashMap<&str, u64> = HashMap::new();
    let mut par: HashMap<&str, OrdCounter> = HashMap::new();
    for m in &c.meths {
        for e in &m.ev {
            *nn.entry(&e.name).or_default() += 1;
            *cc.entry(&e.name).or_default() += e.concat as u64;
            par.entry(&e.name).or_default().add(&e.parent);
        }
    }
    for (a, &n) in &nn {
        if n < 100 {
            continue;
        }
        let top: Vec<(&str, u64)> = par[a]
            .most_common(4)
            .into_iter()
            .filter(|(p, _)| !p.is_empty())
            .collect();
        let Some(&(p, cnt)) = top.first() else {
            continue;
        };
        if cnt as f64 >= 0.5 * n as f64 {
            out.push(json!({"mech": "C6-nested", "A": a, "parent": p,
                "share": pyround(cnt as f64 / n as f64, 3), "n": n}));
        }
        let ca = cc[a];
        if ca as f64 <= 0.1 * n as f64
            && top.iter().map(|x| x.1).sum::<u64>() as f64 >= 0.2 * n as f64
        {
            let parents: Vec<Value> = top
                .iter()
                .map(|(p, c)| json!([p, pyround(*c as f64 / n as f64, 3)]))
                .collect();
            out.push(json!({"mech": "C6-noconcat", "A": a,
                "concat_share": pyround(ca as f64 / n as f64, 3), "parents": parents, "n": n}));
        }
    }

    // ---------- C5 ----------
    type Obj<'a> = Option<(&'a str, &'a str)>;
    let meth_obj: HashMap<i64, (&str, &str)> = c
        .meths
        .iter()
        .map(|m| {
            let md = &c.mods[&m.module];
            (m.id, (opt(&md.category), opt(&md.object)))
        })
        .collect();
    let mut callers: HashMap<&str, u64> = HashMap::new();
    let mut caller_objs: HashMap<&str, HashSet<Obj>> = HashMap::new();
    for (key, caller) in &env.calls {
        *callers.entry(key).or_default() += 1;
        caller_objs
            .entry(key)
            .or_default()
            .insert(meth_obj.get(caller).copied());
    }
    let (mut unused, mut notcalled) = (0u64, 0u64);
    let (mut exp_self, mut exp_total) = (0u64, 0u64);
    for m in &c.meths {
        let md = &c.mods[&m.module];
        let k = format!("{}::{}", md.rel, m.name.to_lowercase());
        let nc = *callers.get(k.as_str()).unwrap_or(&0);
        if !m.export && !handlers.contains(&m.name) && !env.form_handlers.contains(&m.name) {
            notcalled += 1;
            if nc == 0 {
                unused += 1;
            }
        }
        if m.export && opt(&md.category) != "CommonModules" && nc > 0 {
            exp_total += 1;
            let own: Obj = meth_obj.get(&m.id).copied();
            if caller_objs
                .get(k.as_str())
                .is_none_or(|s| s.iter().all(|x| *x == own))
            {
                exp_self += 1;
            }
        }
    }
    out.push(json!({"mech": "C5-unused", "share_called": pyround(1.0 - unused as f64 / notcalled.max(1) as f64, 3),
        "unused": unused, "of": notcalled}));
    out.push(json!({"mech": "C5-export-self-only", "share": pyround(exp_self as f64 / exp_total.max(1) as f64, 3),
        "self_only": exp_self, "of": exp_total}));
    out
}

// ---------------------------------------------------------------- ранжирование

const CAP: f64 = 50.0;

fn kl(q: f64, p: f64) -> f64 {
    let q = q.clamp(1e-12, 1.0 - 1e-12);
    let p = p.clamp(1e-12, 1.0 - 1e-12);
    q * (q / p).ln() + (1.0 - q) * ((1.0 - q) / (1.0 - p)).ln()
}

fn ln10() -> f64 {
    10f64.ln()
}

/// −log10 P(X ≥ c) для X ~ Пуассон(e), граница Чернова.
fn pois_upper(c: f64, e: f64) -> f64 {
    if c <= e || e <= 0.0 {
        return 0.0;
    }
    (c * (c / e).ln() - (c - e)) / ln10()
}

/// −log10 P(X ≤ o) для X ~ Пуассон(e), граница Чернова.
fn pois_lower(o: f64, e: f64) -> f64 {
    if o >= e {
        return 0.0;
    }
    (e - o + if o > 0.0 { o * (o / e).ln() } else { 0.0 }) / ln10()
}

/// −log10 P(доля ≥ q | p).
fn binom(n: f64, q: f64, p: f64) -> f64 {
    if q > p {
        n * kl(q, p) / ln10()
    } else {
        0.0
    }
}

struct Rec {
    mech: String,
    fam: (String, String),
    ctx: String,
    sym: String,
    rec: Value,
    e: f64,
    spread: usize,
    f: f64,
    div: f64,
    score: f64,
}

/// Итог ранжирования: семейства по убыванию оценки и все находки с рангом
/// своего семейства.
pub struct Ranked {
    pub families: Vec<Value>,
    pub members: Vec<Value>,
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn st(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn rank(c: &Corpus, s: &Syms, c1: &[Value], c2: &[Value]) -> Ranked {
    let n = c.meths.len() as f64;
    let nmod = c.mods.len();
    let mut sym_mods: HashMap<u32, HashSet<i64>> = HashMap::new();
    let mut sym_cnt: HashMap<u32, u64> = HashMap::new();
    for (mi, m) in c.meths.iter().enumerate() {
        for &x in &s.mset[mi] {
            sym_mods.entry(x).or_default().insert(m.module);
            *sym_cnt.entry(x).or_default() += 1;
        }
    }
    let name = |x: u32| s.it.name(x);
    let mut base_of: HashMap<&str, Vec<u32>> = HashMap::new();
    for &x in sym_cnt.keys() {
        base_of.entry(base(name(x))).or_default().push(x);
    }
    let cnt = |sy: &str| {
        s.it.get(sy)
            .and_then(|i| sym_cnt.get(&i))
            .copied()
            .unwrap_or(0)
    };
    let mods_n = |sy: &str| {
        s.it.get(sy)
            .and_then(|i| sym_mods.get(&i))
            .map_or(0, HashSet::len)
    };
    let mods_of = |b: &str| -> usize {
        match base_of.get(b) {
            Some(xs) => {
                let mut r: HashSet<i64> = HashSet::new();
                for x in xs {
                    if let Some(ms) = sym_mods.get(x) {
                        r.extend(ms);
                    }
                }
                r.len()
            }
            None => mods_n(b),
        }
    };
    // подпись реализации: набор вызовов метода; одинаковые подписи — копии
    let mut sig_id: HashMap<&Vec<u32>, u32> = HashMap::new();
    let mut sig: Vec<u32> = Vec::with_capacity(s.mset.len());
    for ss in &s.mset {
        let next = sig_id.len() as u32;
        sig.push(*sig_id.entry(ss).or_insert(next));
    }
    let mut sym_sigs: HashMap<u32, HashSet<u32>> = HashMap::new();
    for (mi, ss) in s.mset.iter().enumerate() {
        for &x in ss {
            sym_sigs.entry(x).or_default().insert(sig[mi]);
        }
    }
    let div_sym = |sy: &str| -> f64 {
        let nn = cnt(sy);
        if nn == 0 {
            return 1.0;
        }
        let k =
            s.it.get(sy)
                .and_then(|i| sym_sigs.get(&i))
                .map_or(0, HashSet::len);
        k as f64 / nn as f64
    };
    let handlers = names_in_50(c);
    let mut kgroups: HashMap<String, Vec<u32>> = HashMap::new();
    for (mi, m) in c.meths.iter().enumerate() {
        let md = &c.mods[&m.module];
        kgroups.entry(mt_key(md)).or_default().push(sig[mi]);
        if handlers.contains(&m.name) {
            kgroups
                .entry(format!(
                    "h:{}{}",
                    m.name,
                    if md.is_form { "/form" } else { "" }
                ))
                .or_default()
                .push(sig[mi]);
        }
    }
    let kdiv: HashMap<String, f64> = kgroups
        .into_iter()
        .map(|(k, v)| {
            let u: HashSet<u32> = v.iter().copied().collect();
            (k, u.len() as f64 / v.len() as f64)
        })
        .collect();

    let mut recs: Vec<Rec> = Vec::new();
    let mk = |mech: &str,
              sym: String,
              ctx: String,
              rec: &Value,
              e: f64,
              spread: usize,
              fv: f64,
              div: f64| Rec {
        mech: mech.to_string(),
        fam: (mech.to_string(), sym.clone()),
        ctx,
        sym,
        rec: rec.clone(),
        e,
        spread,
        f: fv,
        div,
        score: 0.0,
    };
    // ----- C1 -----
    let mut by_a: HashMap<u32, HashSet<u32>> = HashMap::new();
    for r in c1 {
        if let (Some(a), Some(b)) = (
            s.it.get(r["A"].as_str().unwrap_or("")),
            s.it.get(r["B"].as_str().unwrap_or("")),
        ) {
            by_a.entry(a).or_default().insert(b);
        }
    }
    let mut pair_mods: HashMap<(u32, u32), HashSet<i64>> = HashMap::new();
    let mut pair_sigs: HashMap<(u32, u32), HashSet<u32>> = HashMap::new();
    let mut pair_n: HashMap<(u32, u32), u64> = HashMap::new();
    for (mi, m) in c.meths.iter().enumerate() {
        let ss: HashSet<u32> = s.mset[mi].iter().copied().collect();
        for &a in &s.mset[mi] {
            let Some(bs) = by_a.get(&a) else { continue };
            for &b in bs {
                if ss.contains(&b) {
                    pair_mods.entry((a, b)).or_default().insert(m.module);
                    pair_sigs.entry((a, b)).or_default().insert(sig[mi]);
                    *pair_n.entry((a, b)).or_default() += 1;
                }
            }
        }
    }
    for r in c1 {
        let (a, b) = (st(&r["A"]), st(&r["B"]));
        let key = (
            s.it.get(&a).unwrap_or(u32::MAX),
            s.it.get(&b).unwrap_or(u32::MAX),
        );
        let e = (r["nA"].as_u64().unwrap_or(0) * cnt(&b)) as f64 / n;
        let pn = *pair_n.get(&key).unwrap_or(&0);
        let div = pair_sigs.get(&key).map_or(0, HashSet::len) as f64 / pn.max(1) as f64;
        let mut x = mk(
            "C1",
            a.clone(),
            b.clone(),
            r,
            pois_upper(f(&r["support"]), e),
            pair_mods.get(&key).map_or(0, HashSet::len),
            f(&r["conf"]),
            div,
        );
        x.fam = ("C1".into(), a);
        recs.push(x);
    }
    // ----- C2..C6 -----
    for r in c2 {
        let m = st(&r["mech"]);
        let a = st(&r["A"]);
        let k = st(&r["K"]);
        let kd = |k: &str, dflt: f64| kdiv.get(k).copied().unwrap_or(dflt);
        let x = if m.starts_with("C2-avoid") {
            let (o, e) = (f(&r["observed"]), f(&r["expected"]));
            mk(
                &m,
                a.clone(),
                k.clone(),
                r,
                pois_lower(o, e),
                mods_n(&a),
                1.0 - o / e,
                kd(&k, div_sym(&a)),
            )
        } else if m == "C2-block-avoid" || m == "C4-nokey" {
            let (o, e) = (f(&r["observed"]), f(&r["expected"]));
            let ctx = if r.get("K").is_some() {
                k.clone()
            } else {
                st(&r["key"])
            };
            mk(
                &m,
                a.clone(),
                ctx,
                r,
                pois_lower(o, e),
                mods_n(&a),
                1.0 - o / e,
                div_sym(&a),
            )
        } else if m == "C2-block-require" {
            let (nn, o) = (f(&r["n"]), f(&r["observed"]));
            mk(
                &m,
                a.clone(),
                k.clone(),
                r,
                binom(nn, o / nn, f(&r["base"])),
                mods_n(&a),
                o / nn,
                div_sym(&a),
            )
        } else if m == "C2-require" {
            let (nk, o) = (f(&r["nK"]), f(&r["observed"]));
            let p0 = cnt(&a) as f64 / n;
            mk(
                &m,
                a.clone(),
                k.clone(),
                r,
                binom(nk, o / nk, p0),
                mods_n(&a),
                o / nk,
                kd(&k, 1.0),
            )
        } else if m == "C3-wrapper" {
            let (out_p, w) = (f(&r["P_outside"]), f(&r["wrapper_calls"]));
            let nn = out_p + w;
            let ws: Vec<String> = r["wrappers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(st)
                .collect();
            let mut sp: HashSet<i64> = HashSet::new();
            for x in &ws {
                if let Some(ms) = s.it.get(x).and_then(|i| sym_mods.get(&i)) {
                    sp.extend(ms);
                }
            }
            let div = ws
                .iter()
                .map(|x| div_sym(x))
                .fold(None, |acc: Option<f64>, v| {
                    Some(acc.map_or(v, |a| a.min(v)))
                })
                .unwrap_or(1.0);
            mk(
                &m,
                st(&r["P"]),
                String::new(),
                r,
                binom(nn, w / nn, 0.5),
                sp.len(),
                w / nn,
                div,
            )
        } else if m == "C3-similar" {
            let (cp, cq) = (f(&r["P_calls"]), f(&r["Q_calls"]));
            let (p, q) = (st(&r["P"]), st(&r["Q"]));
            let pb = base(&p).to_lowercase();
            let qb = base(&q).to_lowercase();
            let last = |x: &str| x.rsplit('.').next().unwrap_or("").to_string();
            if pb == qb || last(&pb) == last(&qb) || qb.starts_with("*.") {
                continue; // не альтернатива: тот же метод в другом регистре / с другим аргументом
            }
            let nn = cp + cq;
            mk(
                &m,
                p,
                q.clone(),
                r,
                binom(nn, cq / nn, 0.5),
                mods_of(&q),
                (cq / nn) * f(&r["jaccard"]),
                div_sym(&q),
            )
        } else if m == "C4-arg" {
            let (nn, sh) = (f(&r["n"]), f(&r["share"]));
            let ctx = r["pos"].to_string();
            mk(
                &m,
                a.clone(),
                ctx,
                r,
                binom(nn, sh, 0.5),
                mods_of(&a),
                sh,
                div_sym(&a),
            )
        } else if m == "C6-nested" {
            let (nn, sh) = (f(&r["n"]), f(&r["share"]));
            mk(
                &m,
                a.clone(),
                st(&r["parent"]),
                r,
                binom(nn, sh, 0.5),
                mods_of(&a),
                sh,
                div_sym(&a),
            )
        } else if m == "C6-noconcat" {
            let (nn, sh) = (f(&r["n"]), 1.0 - f(&r["concat_share"]));
            mk(
                &m,
                a.clone(),
                String::new(),
                r,
                binom(nn, sh, 0.5),
                mods_of(&a),
                sh,
                div_sym(&a),
            )
        } else if m == "C5-unused" {
            mk(
                &m,
                String::new(),
                String::new(),
                r,
                CAP,
                nmod,
                f(&r["share_called"]),
                1.0,
            )
        } else if m == "C5-export-self-only" {
            mk(
                &m,
                String::new(),
                String::new(),
                r,
                CAP,
                nmod,
                f(&r["share"]),
                1.0,
            )
        } else {
            continue;
        };
        recs.push(x);
    }
    // ----- D: штраф частым контекстам и символам -----
    let mut by_ctx: HashMap<(String, String), u64> = HashMap::new();
    let mut by_sym: HashMap<(String, String), u64> = HashMap::new();
    for r in &recs {
        *by_ctx.entry((r.mech.clone(), r.ctx.clone())).or_default() += 1;
        *by_sym.entry((r.mech.clone(), r.sym.clone())).or_default() += 1;
    }
    let log_n = (nmod as f64).ln();
    for r in &mut recs {
        let e = (r.e * r.div).min(CAP) / CAP;
        let sp = (1.0 + r.spread as f64).ln() / log_n;
        let d = 1.0
            / (1.0 + (by_ctx[&(r.mech.clone(), r.ctx.clone())] as f64).ln())
            / (1.0 + (by_sym[&(r.mech.clone(), r.sym.clone())] as f64).ln());
        r.score = e * sp * d * r.f.max(0.0);
    }
    // ----- семейства -----
    let mut fam: HashMap<(String, String), usize> = HashMap::new();
    let mut fam_order: Vec<(String, String)> = Vec::new();
    for (i, r) in recs.iter().enumerate() {
        match fam.get(&r.fam) {
            Some(&j) if r.score <= recs[j].score => {}
            Some(_) => {
                fam.insert(r.fam.clone(), i);
            }
            None => {
                fam.insert(r.fam.clone(), i);
                fam_order.push(r.fam.clone());
            }
        }
    }
    let mut order0: Vec<usize> = fam_order.iter().map(|k| fam[k]).collect();
    order0.sort_by(|&a, &b| recs[b].score.total_cmp(&recs[a].score));
    // зеркальные пары C1: A⇒B и B⇒A — одно семейство на месте лучшей
    let mut order: Vec<usize> = Vec::new();
    let mut alias: HashMap<(String, String), (String, String)> = HashMap::new();
    let mut seen: HashMap<BTreeSet<String>, (String, String)> = HashMap::new();
    for i in order0 {
        let r = &recs[i];
        if r.mech == "C1" {
            let key: BTreeSet<String> = [st(&r.rec["A"]), st(&r.rec["B"])].into_iter().collect();
            if let Some(to) = seen.get(&key) {
                alias.insert(r.fam.clone(), to.clone());
                continue;
            }
            seen.insert(key, r.fam.clone());
        }
        order.push(i);
    }
    let mut rank_of: HashMap<(String, String), usize> = HashMap::new();
    let families: Vec<Value> = order
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let r = &recs[i];
            rank_of.insert(r.fam.clone(), k + 1);
            json!({"rank": k + 1, "score": pyround(r.score, 5), "mech": r.mech, "rec": r.rec})
        })
        .collect();
    for (from, to) in alias {
        let rk = rank_of[&to];
        rank_of.insert(from, rk);
    }
    let members = recs
        .iter()
        .map(|r| json!({"rank": rank_of[&r.fam], "mech": r.mech, "rec": r.rec, "score": pyround(r.score, 5)}))
        .collect();
    Ranked { families, members }
}

// ---------------------------------------------------------------- добыча

/// Итог добычи.
pub struct Mined {
    pub methods: usize,
    pub events: usize,
    pub modules: usize,
    pub read_errors: usize,
    pub c1: Vec<Value>,
    pub c2: Vec<Value>,
    pub ranked: Ranked,
}

/// Добыть кандидатов по индексу и исходникам конфигурации. `global_methods`
/// — методы глобального контекста из справки платформы (для C3-similar).
pub fn mine(
    conn: &Connection,
    source: &Path,
    global_methods: HashSet<String>,
) -> Result<Mined, String> {
    let c = corpus(conn, source)?;
    let s = syms(&c);
    let en = env(conn, global_methods)?;
    let c1 = mine_c1(&c, &s);
    let c2 = mine_c2_c6(&c, &s, &c1, &en);
    let ranked = rank(&c, &s, &c1, &c2);
    Ok(Mined {
        methods: c.meths.len(),
        events: c.meths.iter().map(|m| m.ev.len()).sum(),
        modules: c.mods.len(),
        read_errors: c.read_errors,
        c1,
        c2,
        ranked,
    })
}

// ---------------------------------------------------------------- файл

/// Файл кандидатов рядом с индексом: `<имя индекса>.practices-calls.json`.
pub fn sidecar(db: &Path) -> PathBuf {
    let stem = db.file_stem().and_then(|s| s.to_str()).unwrap_or("index");
    db.with_file_name(format!("{stem}.practices-calls.json"))
}

/// Добыть и записать файл кандидатов. Возвращает путь и сводку. `dump` —
/// каталог для промежуточных файлов в форме пилота (`c1.jsonl`,
/// `c2c5.jsonl`, `ranked.jsonl`, `members.jsonl`) — для сверки.
pub fn mine_to_file(
    db: &Path,
    out: Option<&Path>,
    dump: Option<&Path>,
) -> Result<(PathBuf, Value), String> {
    let conn = crate::sql::open_readonly(db).map_err(|e| e.to_string())?;
    let source = meta(&conn, "source_path").ok_or("в индексе нет source_path")?;
    // Без справки платформы C3-similar неполон — это говорится в сводке, а
    // не прячется: остальные механизмы от справки не зависят.
    let (gm, help_note) = match crate::platform_help::global_context_methods() {
        Ok(v) => (v.into_iter().collect(), Value::Null),
        Err(e) => (
            HashSet::new(),
            json!(format!(
                "справка платформы недоступна ({e}): C3-similar без методов глобального контекста"
            )),
        ),
    };
    let t0 = std::time::Instant::now();
    let m = mine(&conn, Path::new(&source), gm)?;
    let secs = pyround(t0.elapsed().as_secs_f64(), 1);
    let mut by_mech: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    by_mech.insert("C1".into(), m.c1.len());
    for r in &m.c2 {
        *by_mech.entry(st(&r["mech"])).or_default() += 1;
    }
    let summary = json!({
        "modules": m.modules,
        // модули из индекса, не прочитанные с диска (нет файла, не UTF-8):
        // их методы в добычу не попали
        "read_errors": m.read_errors,
        "methods": m.methods,
        "events": m.events,
        "findings": by_mech,
        "families": m.ranked.families.len(),
        "platform_help": help_note,
        "seconds": secs,
    });
    if let Some(d) = dump {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        let lines = |v: &[Value]| v.iter().map(|x| x.to_string() + "\n").collect::<String>();
        for (name, v) in [
            ("c1.jsonl", &m.c1),
            ("c2c5.jsonl", &m.c2),
            ("ranked.jsonl", &m.ranked.families),
            ("members.jsonl", &m.ranked.members),
        ] {
            std::fs::write(d.join(name), lines(v)).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    let doc = json!({
        "format": FORMAT,
        "direction": "calls",
        "experimental": true,
        "generated_by": env!("CARGO_PKG_VERSION"),
        "index_built_at": meta(&conn, "built_at"),
        "source_path": source,
        "summary": summary,
        // Только семейства: все находки (`members`, на БП — 90 тыс.) инструменту
        // не нужны, а файл он разбирает на каждом вызове. Они — в --dump.
        "families": m.ranked.families,
    });
    let path = out.map(Path::to_path_buf).unwrap_or_else(|| sidecar(db));
    let text = serde_json::to_string(&doc).map_err(|e| e.to_string())?;
    // Через временный файл: работающий сервер может читать файл в эту же
    // минуту, и наполовину записанный JSON стал бы для агента ошибкой.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("не записан {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("не записан {}: {e}", path.display()))?;
    Ok((path, summary))
}

// ---------------------------------------------------------------- инструмент

/// Отдать кандидатов «Вызовов» из файла рядом с индексом.
pub fn candidates(conn: &Connection, args: &Value) -> Result<Value, String> {
    let db = conn
        .path()
        .filter(|p| !p.is_empty())
        .ok_or("индекс без файла: кандидатам негде лежать")?;
    let path = sidecar(Path::new(db));
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "кандидатов нет: добыча «Вызовов» по этому индексу не запускалась. Это не значит, что \
             практик нет. Запуск: gyrfalcon practices mine-calls --db {db}"
        )
    })?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if doc["format"] != FORMAT {
        return Err(format!(
            "файл {} другого формата ({}), нужен {FORMAT} — перезапустите practices mine-calls",
            path.display(),
            doc["format"]
        ));
    }
    let mech = args
        .get("mech")
        .and_then(Value::as_str)
        .map(str::to_uppercase);
    if let Some(m) = mech.as_deref().filter(|m| {
        !["C1", "C2", "C3", "C4", "C5", "C6"]
            .iter()
            .any(|p| m.starts_with(p))
    }) {
        return Err(format!(
            "mech {m} не из «Вызовов» (C1–C6); K1, K2 — это direction: \"code\""
        ));
    }
    let needle = args
        .get("contains")
        .and_then(Value::as_str)
        .map(str::to_lowercase);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 1000) as usize;
    let mut rows = Vec::new();
    for r in doc["families"].as_array().into_iter().flatten() {
        let m = r["mech"].as_str().unwrap_or("");
        if mech
            .as_ref()
            .is_some_and(|x| !m.to_uppercase().starts_with(x.as_str()))
        {
            continue;
        }
        // по значениям символов и контекстов, а не по JSON целиком: иначе
        // «conf» или «parent» совпадают с именами полей у всех записей
        if let Some(nd) = &needle {
            let rec = &r["rec"];
            let hit = ["A", "B", "K", "P", "Q", "parent", "key", "value"]
                .iter()
                .filter_map(|k| rec[k].as_str())
                .chain(
                    rec["wrappers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str),
                )
                .any(|v| v.to_lowercase().contains(nd.as_str()));
            if !hit {
                continue;
            }
        }
        let weak = m == "C3-similar";
        rows.push(json!([r["rank"], r["score"], m, r["rec"], weak]));
    }
    let total = rows.len();
    rows.truncate(limit);
    let built_now = meta(conn, "built_at");
    let outdated = doc["index_built_at"].as_str() != built_now.as_deref();
    Ok(json!({
        "experimental": true,
        "direction": "calls",
        "kind": "кандидаты, а не правила",
        "precision": PRECISION,
        "weak_note": WEAK_C3_SIMILAR,
        "read_as": "Семейства находок по убыванию оценки (значимость × разброс по модулям × \
                    отличие × сила). C1 — A требует B; C2 — A избегают/требуют в контексте K; \
                    C3 — платформенный P вытеснен обёрткой или Q; C4 — форма аргумента; \
                    C5 — граф; C6 — склейка и вложенность. Повод проверить по стандартам \
                    (standards_1c), а не норма.",
        "mined_by": doc["generated_by"],
        "summary": doc["summary"],
        "outdated": outdated,
        "outdated_note": if outdated { json!("индекс пересобран после добычи — кандидаты могли устареть") } else { Value::Null },
        "columns": ["rank", "score", "mech", "rec", "weak"],
        "rows": rows,
        "returned": rows.len(),
        "total": total,
        "truncated": total > limit,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_plain(c: &[&str]) -> String {
        resolve_chain(c, &HashSet::new(), &HashMap::new(), &HashSet::new())
    }

    #[test]
    fn округление_как_у_питона() {
        assert_eq!(pyround(0.25, 1), 0.2, "точная половина — к чётному");
        assert_eq!(
            pyround(0.35, 1),
            0.3,
            "0.35 в двоичном чуть меньше половины"
        );
        assert_eq!(pyround(2.675, 2), 2.67);
        assert_eq!(pyround(12.75, 1), 12.8);
        assert_eq!(pyround(40.0, 1), 40.0);
    }

    #[test]
    fn форма_литерала_и_ключи() {
        assert_eq!(
            lit_desc("\"ru = 'Ошибка %1'; en = 'Error %1'\""),
            "S:a='…';a='…'~a = 'a %9'; a = 'a %9'|k=en,ru"
        );
        assert_eq!(lit_desc("\"Дата\""), "S:a~a");
    }

    #[test]
    fn события_контекст_и_аргументы() {
        let ev = parse_method(
            "Попытка\n НачатьТранзакцию();\n Х.Записать(Истина);\nИсключение\n ОтменитьТранзакцию();\nКонецПопытки;\n\
             Для Каждого С Из Список Цикл\n Если С.Ф(\"а\", 1) Тогда Сообщить(НСтр(\"ru = 'x'\") + Т); КонецЕсли;\nКонецЦикла;\n\
             З = Новый Структура(\"А\");",
            &resolve_plain,
        );
        let v: Vec<(&str, &str, &str)> = ev
            .iter()
            .map(|e| (e.name.as_str(), e.ctx.as_str(), e.ak.as_str()))
            .collect();
        assert_eq!(
            v,
            vec![
                ("НачатьТранзакцию", "T:1", "-"),
                ("*.Записать", "T:1", "=истина"),
                ("ОтменитьТранзакцию", "X:1", "-"),
                ("*.Ф", "CL", "S"),
                ("Сообщить", "BL", "E"),
                ("НСтр", "BL", "S"),
                ("Новый Структура", "", "S"),
            ]
        );
        assert_eq!(ev[1].sym(), "*.Записать(истина)");
        assert_eq!(ev[3].args[1], "N");
        assert_eq!(ev[4].args, vec!["E".to_string()]);
        assert_eq!(ev[5].parent, "Сообщить", "НСтр — аргумент Сообщить");
        assert!(ev[5].concat, "НСтр(…) + Т — склейка");
    }

    #[test]
    fn аргумент_вызов_и_цепочка() {
        let ev = parse_method("Ф(А.Б.В, Г(1), Г(1) + 2, \"x\" + \"y\");", &resolve_plain);
        assert_eq!(ev[0].args, vec!["I:в", "C:Г", "E", "E"]);
    }

    #[test]
    fn имена_по_общим_модулям_и_параметрам() {
        let common: HashMap<String, String> = [(
            "общегоназначения".to_string(),
            "ОбщегоНазначения".to_string(),
        )]
        .into();
        let gobj: HashSet<String> = GLOBAL_OBJ
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let ps: HashSet<String> = ["параметры".to_string()].into();
        let r = |c: &[&str]| resolve_chain(c, &ps, &common, &gobj);
        assert_eq!(r(&["общегоНазначения", "Х"]), "ОбщегоНазначения.Х");
        assert_eq!(r(&["Параметры", "Свойство"]), "param:Параметры.Свойство");
        assert_eq!(r(&["Справочники", "Банки", "Найти"]), "Справочники.*.Найти");
        assert_eq!(r(&["Запрос", "Выполнить"]), "*.Выполнить");
    }

    #[test]
    fn вызов_исключения_как_у_пилота() {
        let ev = parse_method(
            "ВызватьИсключение(\"x\");\nВызватьИсключение \"y\";",
            &resolve_plain,
        );
        assert_eq!(ev[0].ak, "E", "пилот смотрит на `(`, а не на аргумент");
        assert_eq!(ev[1].ak, "-");
    }

    /// Мини-конфигурация для C1: 30 методов с парой НачатьТранзакцию ⇒
    /// ЗафиксироватьТранзакцию в 30 модулях и 300 прочих.
    fn корпус(dir: &Path) -> Connection {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let db = dir.join("t-index.db");
        let _ = std::fs::remove_file(&db);
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE modules (id INTEGER PRIMARY KEY, rel_path TEXT, category TEXT,
               object_name TEXT, module_type TEXT, is_form INTEGER);
             CREATE TABLE methods (id INTEGER PRIMARY KEY, module_id INTEGER, name TEXT, type TEXT,
               is_export INTEGER, params TEXT, line INTEGER, end_line INTEGER);
             CREATE TABLE calls (id INTEGER PRIMARY KEY, caller_id INTEGER, callee_name TEXT,
               line INTEGER, callee_key TEXT, resolution TEXT, confidence REAL);
             CREATE TABLE scheduled_jobs (handler_module TEXT, handler_procedure TEXT);
             CREATE TABLE event_subscriptions (handler_module TEXT, handler_procedure TEXT);
             CREATE TABLE form_elements (handler TEXT);
             CREATE TABLE index_meta (key TEXT, value TEXT);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO index_meta VALUES ('source_path', ?1), ('built_at', '1')",
            [src.to_str().unwrap()],
        )
        .unwrap();
        for i in 0..60 {
            let rel = format!("m{i}.bsl");
            let body = if i < 30 {
                "Процедура Провести()\n\tНачатьТранзакцию();\n\tХ();\n\tЗафиксироватьТранзакцию();\nКонецПроцедуры\n"
            } else {
                "Процедура Провести()\n\tХ();\n\tУ();\nКонецПроцедуры\n"
            };
            let more: String = (0..5)
                .map(|k| format!("Процедура П{k}()\n\tУ();\n\tZ{k}();\nКонецПроцедуры\n"))
                .collect();
            std::fs::write(src.join(&rel), format!("{body}{more}")).unwrap();
            c.execute(
                "INSERT INTO modules VALUES (?1, ?2, 'Documents', ?3, 'ObjectModule', 0)",
                rusqlite::params![i + 1, rel, format!("Д{i}")],
            )
            .unwrap();
            let first_end = if i < 30 { 5 } else { 4 };
            c.execute(
                "INSERT INTO methods (module_id, name, is_export, params, line, end_line)
                 VALUES (?1, 'Провести', 0, '()', 1, ?2)",
                rusqlite::params![i + 1, first_end],
            )
            .unwrap();
            for k in 0..5 {
                let line = first_end + 1 + k * 4;
                c.execute(
                    "INSERT INTO methods (module_id, name, is_export, params, line, end_line)
                     VALUES (?1, ?2, 0, '()', ?3, ?4)",
                    rusqlite::params![i + 1, format!("П{k}"), line, line + 3],
                )
                .unwrap();
            }
        }
        c
    }

    #[test]
    fn находит_связку_и_пишет_файл() {
        let dir = std::env::temp_dir().join("gyrfalcon-practice-calls-mine");
        let _ = std::fs::remove_dir_all(&dir);
        let c = корпус(&dir);
        let m = mine(&c, &dir.join("src"), HashSet::new()).unwrap();
        assert_eq!(m.methods, 360);
        let hit =
            m.c1.iter()
                .find(|r| r["A"] == "НачатьТранзакцию" && r["B"] == "ЗафиксироватьТранзакцию")
                .unwrap_or_else(|| panic!("нет связки: {:#?}", m.c1));
        assert_eq!(hit["support"], 30);
        assert_eq!(hit["conf"], 1.0);
        let rels: Vec<&str> = hit["relations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert!(
            rels.contains(&"A_first") && rels.contains(&"B_after_last_A"),
            "{rels:?}"
        );
        assert!(m.ranked.families.iter().any(|f| f["mech"] == "C1"));

        let db = dir.join("t-index.db");
        let нет = candidates(&c, &json!({})).unwrap_err();
        assert!(нет.contains("не запускалась"), "{нет}");
        let (path, _) = mine_to_file(&db, None, None).unwrap();
        assert_eq!(path, dir.join("t-index.practices-calls.json"));
        let v = candidates(&c, &json!({"mech": "c1", "contains": "транзакц"})).unwrap();
        assert_eq!(v["experimental"], true);
        assert_eq!(v["outdated"], false);
        assert!(v["total"].as_u64().unwrap() >= 1, "{v:#}");
        let чужой = candidates(&c, &json!({"mech": "K1"})).unwrap_err();
        assert!(
            чужой.contains("direction"),
            "механизм другого направления — отказ: {чужой}"
        );
        let поле = candidates(&c, &json!({"contains": "relations"})).unwrap();
        assert_eq!(поле["total"], 0, "имя поля JSON — не совпадение");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            doc.get("members").is_none(),
            "все находки — только в --dump"
        );
        assert_eq!(doc["summary"]["read_errors"], 0);
        c.execute("UPDATE index_meta SET value='2' WHERE key='built_at'", [])
            .unwrap();
        assert_eq!(candidates(&c, &json!({})).unwrap()["outdated"], true);
    }
}
