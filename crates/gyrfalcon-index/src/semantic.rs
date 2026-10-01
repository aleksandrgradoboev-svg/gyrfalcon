//! Семантический слой: векторы токенов и поиск по смыслу.
//!
//! # Что решает
//!
//! Лексический поиск (FTS5) находит только точное вхождение слова. Вопрос
//! «где считается себестоимость» не найдёт `РасчётСтоимостиПартий`, потому
//! что слова «себестоимость» в имени нет. Семантика закрывает именно этот
//! разрыв — и только его: там, где слово совпадает, лексика точнее и дешевле.
//!
//! # Устройство (решение Р-006)
//!
//! Вектор токена берётся **двухпутевой** функцией, и пути дополняют друг
//! друга, а не конкурируют:
//!
//! | Условие | Путь |
//! |---|---|
//! | токен есть в словаре | готовый int8-вектор из таблицы |
//! | токена нет | разреженный вектор random indexing от самой строки |
//!
//! **Инференса при индексации нет ни на одном пути.** Словарь считается
//! офлайн, отдельным прогоном. Это прямое условие вехи: модель, зовомая во
//! время сборки, убила бы скорость — первый из трёх критериев замены — и
//! привязала бы индексатор к аптайму сервиса, который в контуре гаснет
//! по вечерам.
//!
//! **Пустой словарь — штатное состояние, а не поломка.** На 28.08.2026 он
//! пуст: эндпоинт эмбеддингов локального роутера отвечает 404. Всё уходит
//! по sparse-пути, поиск работает, качество ниже. Отличать эти два режима
//! обязан вызывающий — для того `SemanticStats::dict_hits` и считается.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::Result;

/// Размерность вектора — как у модели словаря, без понижения.
///
/// # Выбрана замером, а не соображением об экономии
///
/// Сначала здесь стояло 128 «чтобы словарь весил 2,4 МБ». Замер 28.08.2026
/// на 300 настоящих именах методов БП показал цену этой экономии: сравнивалась
/// близость, посчитанная понижённой размерностью, с близостью по полному
/// вектору модели (jina-embeddings-v3, 1024).
///
/// | Размерность + int8 | Корреляция с полным | Средняя ошибка косинуса |
/// |---|---|---|
/// | 1024 | **0,9994** | 0,0031 |
/// | 512 | 0,9445 | 0,0268 |
/// | 256 | 0,8823 | 0,0423 |
/// | 128 | **0,8130** | 0,0509 |
/// | 64 | 0,7069 | 0,0976 |
///
/// То есть квантование в int8 при полной размерности почти бесплатно, а вот
/// случайная проекция 1024 → 128 съедает пятую часть сходства. Цена отказа
/// от неё — словарь 19 МБ вместо 2,4 МБ при индексе в 1,7 ГБ, то есть ничто.
///
/// **Константа связана с моделью словаря.** Сменится модель — сменится и она,
/// а старый словарь станет несовместим: длина блоба проверяется при загрузке.
pub const DIM: usize = 1024;

/// Сколько позиций заполняет random indexing у незнакомого токена.
///
/// Разреженность — не экономия, а свойство метода: случайные векторы почти
/// ортогональны, пока ненулевых позиций мало относительно размерности.
const SPARSE_POSITIONS: usize = 8;

/// Вектор токена: int8 фиксированной длины.
pub type Vector = [i8; DIM];

/// Откуда взялся вектор — словарь или хэш.
///
/// Нужен не для отчёта, а для честности выдачи: ответ, собранный целиком
/// из sparse-векторов, слабее ответа по словарю, и вызывающий обязан иметь
/// возможность это увидеть.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorSource {
    /// Готовый вектор из предпосчитанного словаря.
    Dictionary,
    /// Random indexing от строки токена.
    Hashed,
}

/// Словарь векторов, загруженный в память.
///
/// Пустой словарь допустим и означает работу целиком по sparse-пути.
#[derive(Debug, Default)]
pub struct Dictionary {
    vectors: HashMap<String, Vector>,
}

impl Dictionary {
    /// Пустой словарь: всё пойдёт по random indexing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Прочитать словарь из индекса.
    ///
    /// Отсутствие таблицы — не ошибка: индекс, собранный без семантики,
    /// остаётся годным, просто поиск по нему пойдёт по хэшам.
    pub fn load(conn: &Connection) -> Result<Self> {
        let exists: bool = conn
            .prepare(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='semantic_dictionary'",
            )?
            .exists([])?;
        if !exists {
            return Ok(Self::empty());
        }
        let mut st = conn.prepare("SELECT token, vector FROM semantic_dictionary")?;
        let mut vectors = HashMap::new();
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (token, blob) = row?;
            if blob.len() != DIM {
                // Блоб не той длины — это порча, а не «почти вектор».
                // Молча дополнять нулями нельзя: получится вектор, который
                // ни на что не похож, но выглядит настоящим.
                continue;
            }
            let mut v = [0i8; DIM];
            for (i, b) in blob.iter().enumerate() {
                v[i] = *b as i8;
            }
            vectors.insert(token, v);
        }
        Ok(Self { vectors })
    }

    /// Число токенов в словаре.
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// Пуст ли словарь.
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// Вектор токена: словарь, иначе random indexing.
    ///
    /// Это и есть двухпутевая функция из Р-006. Она **всегда** возвращает
    /// вектор — незнакомый токен получает свой, устойчиво выводимый из строки.
    pub fn vector(&self, token: &str) -> (Vector, VectorSource) {
        match self.vectors.get(token) {
            Some(v) => (*v, VectorSource::Dictionary),
            None => (random_indexing(token), VectorSource::Hashed),
        }
    }
}

/// Разреженный вектор от строки: 8 позиций, знак из бита хэша.
///
/// # Почему свой хэш, а не крейт
///
/// Нужен не криптографический и не быстрейший хэш, а **навсегда неизменный**:
/// векторы, разложенные по индексам, обязаны совпадать между сборками и
/// версиями. Обновление чужого крейта, сменившее алгоритм, молча сдвинуло бы
/// весь словарь, и поиск начал бы врать без единой ошибки сборки. FNV-1a
/// описан в двух строках и не изменится никогда — зависимость этого не даёт.
fn random_indexing(token: &str) -> Vector {
    let mut v = [0i8; DIM];
    let bytes = token.as_bytes();
    // Длина подмешивается в сид, а сид умножается на нечётную константу.
    //
    // Замер 28.08.2026 на живом словаре БП (18 893 токена) дал 6 коллизий,
    // и ВСЕ ШЕСТЬ — на двухсимвольных токенах (`12`==`qr`, `25`==`ru`, …).
    // Причина не в FNV, а в том, как он звался: у короткой строки состояние
    // меняется всего дважды, поэтому восемь сидов вида `base + k` давали
    // почти одинаковые хэши и позиции вырождались. Совпавший вектор — это
    // косинус 1,0 между несвязанными словами, то есть ложное «похоже»,
    // неотличимое от настоящего.
    let base = fnv1a(bytes) ^ ((bytes.len() as u64).wrapping_mul(FNV_PRIME));
    for k in 0..SPARSE_POSITIONS {
        // Каждая позиция — свой хэш, иначе восемь значений окажутся
        // соседними битами одного числа и перестанут быть независимыми.
        let h = fnv1a_seeded(bytes, base ^ (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let pos = (h % DIM as u64) as usize;
        let sign: i8 = if (h >> 63) & 1 == 1 { -1 } else { 1 };
        // Складываем, а не присваиваем: две позиции могут совпасть,
        // и тогда их вклад должен сложиться, а не потеряться.
        v[pos] = v[pos].saturating_add(sign);
    }
    v
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_seeded(bytes, FNV_OFFSET)
}

fn fnv1a_seeded(bytes: &[u8], seed: u64) -> u64 {
    let mut h = seed;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Косинусная близость двух векторов, приведённая к диапазону 0..1.
///
/// # Нормировка обязательна, и это не вкус
///
/// Опыт контура 10.08.2026: любой score, идущий в взвешенную сумму, обязан
/// быть нормирован по выдаче — иначе слагаемое с узким разбросом (а косинус
/// именно таков) молча проигрывает всем остальным, и формула ранжирования
/// выглядит рабочей, будучи сломанной. Здесь сделан первый шаг — приведение
/// к общей шкале; нормировку **по выдаче** делает ранжирование, см.
/// [`rank_normalized`].
pub fn cosine(a: &Vector, b: &Vector) -> f32 {
    let mut dot = 0i32;
    let mut na = 0i32;
    let mut nb = 0i32;
    for i in 0..DIM {
        dot += a[i] as i32 * b[i] as i32;
        na += a[i] as i32 * a[i] as i32;
        nb += b[i] as i32 * b[i] as i32;
    }
    if na == 0 || nb == 0 {
        return 0.0;
    }
    let c = dot as f32 / ((na as f32).sqrt() * (nb as f32).sqrt());
    // Из -1..1 в 0..1: отрицательная близость для поиска значит «не похоже»,
    // а не «похоже наоборот».
    ((c + 1.0) / 2.0).clamp(0.0, 1.0)
}

/// Нормировать оценки по выдаче: min-max в 0..1.
///
/// Причина — та же ошибка 10.08.2026. Пока оценки не приведены к общему
/// разбросу **внутри конкретной выдачи**, складывать их с лексическими
/// нельзя: сигнал с узким разбросом исчезает в сумме.
///
/// Вырожденный случай (все оценки равны) даёт 1.0 всем, а не деление на ноль:
/// «все одинаково хороши» — это не «все нулевые».
pub fn rank_normalized(scores: &mut [f32]) {
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for s in scores.iter() {
        lo = lo.min(*s);
        hi = hi.max(*s);
    }
    let span = hi - lo;
    if span <= f32::EPSILON {
        scores.iter_mut().for_each(|s| *s = 1.0);
        return;
    }
    scores.iter_mut().for_each(|s| *s = (*s - lo) / span);
}

/// Статистика семантического этапа сборки.
#[derive(Debug, Default, Clone, Copy)]
pub struct SemanticStats {
    /// Уникальных токенов корпуса.
    pub tokens: u64,
    /// Из них нашлись в словаре.
    pub dict_hits: u64,
    /// Имён, разобранных на токены.
    pub names: u64,
    /// Миллисекунд на этап.
    pub ms: u64,
}

impl SemanticStats {
    /// Доля токенов, накрытых словарём — прямое требование контрольной точки 4.
    pub fn coverage(&self) -> f64 {
        if self.tokens == 0 {
            return 0.0;
        }
        self.dict_hits as f64 / self.tokens as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn хэш_устойчив_между_вызовами() {
        // Главное свойство: один и тот же токен всегда даёт один вектор.
        // Если это сломается, индекс и запрос разъедутся молча.
        assert_eq!(random_indexing("заказ"), random_indexing("заказ"));
    }

    #[test]
    fn разные_токены_дают_разные_векторы() {
        assert_ne!(random_indexing("заказ"), random_indexing("клиента"));
    }

    #[test]
    fn вектор_не_нулевой() {
        // Нулевой вектор дал бы косинус 0 ко всему и выглядел бы как
        // «ничего не найдено» — то есть отказ, неотличимый от факта.
        let v = random_indexing("себестоимость");
        assert!(v.iter().any(|x| *x != 0));
    }

    #[test]
    fn косинус_сам_с_собой_единица() {
        let v = random_indexing("проведение");
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn косинус_нулевого_вектора_ноль_а_не_паника() {
        let z = [0i8; DIM];
        let v = random_indexing("заказ");
        assert_eq!(cosine(&z, &v), 0.0);
    }

    #[test]
    fn разные_токены_почти_ортогональны() {
        // Свойство random indexing: случайные разреженные векторы дают
        // близость около середины шкалы (0,5 после приведения), а не 1.
        let a = random_indexing("номенклатура");
        let b = random_indexing("контрагент");
        let c = cosine(&a, &b);
        assert!(
            (0.2..0.8).contains(&c),
            "близость {c} не похожа на ортогональность"
        );
    }

    #[test]
    fn пустой_словарь_отправляет_всё_в_хэш() {
        let d = Dictionary::empty();
        let (v, src) = d.vector("заказ");
        assert_eq!(src, VectorSource::Hashed);
        assert_eq!(v, random_indexing("заказ"));
    }

    #[test]
    fn словарь_имеет_приоритет_над_хэшем() {
        let mut d = Dictionary::empty();
        let свой = [7i8; DIM];
        d.vectors.insert("заказ".into(), свой);
        let (v, src) = d.vector("заказ");
        assert_eq!(src, VectorSource::Dictionary);
        assert_eq!(v, свой);
    }

    #[test]
    fn нормировка_растягивает_на_ноль_один() {
        let mut s = [0.80, 0.82, 0.84];
        rank_normalized(&mut s);
        assert_eq!(s[0], 0.0);
        assert_eq!(s[2], 1.0);
        assert!((s[1] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn нормировка_равных_не_делит_на_ноль() {
        let mut s = [0.5, 0.5, 0.5];
        rank_normalized(&mut s);
        assert!(s.iter().all(|x| *x == 1.0));
    }

    #[test]
    fn покрытие_пустого_словаря_ноль_а_не_паника() {
        let st = SemanticStats {
            tokens: 0,
            ..Default::default()
        };
        assert_eq!(st.coverage(), 0.0);
    }

    #[test]
    fn покрытие_считается_долей() {
        let st = SemanticStats {
            tokens: 100,
            dict_hits: 25,
            ..Default::default()
        };
        assert!((st.coverage() - 0.25).abs() < 1e-9);
    }
}

/// Найденное семантическим поиском.
#[derive(Debug, Clone)]
pub struct Hit {
    /// `method` или `object`.
    pub kind: String,
    /// Строка в исходной таблице (`methods.id` / `object_synonyms.id`).
    pub ref_id: i64,
    /// Имя сущности (у объектов — вместе с синонимом).
    pub name: String,
    /// Близость 0..1 после нормировки по выдаче.
    pub score: f32,
    /// Сырая близость до нормировки — чтобы отличать «лучший из плохих»
    /// от «лучшего вообще»: нормировка всегда даёт единицу первому месту,
    /// даже когда весь список — мусор.
    pub raw: f32,
}

/// Искать по смыслу: строка запроса → похожие имена.
///
/// # Как считается
///
/// Запрос токенизируется тем же кодом, что и имена при сборке, каждый токен
/// получает вектор двухпутевой функцией (словарь → хэш), веса берутся из
/// `semantic_tokens.idf` — из того же корпуса, с которым сравниваем.
///
/// # Ограничение названо прямо
///
/// Перебор полный: 545 тысяч векторов на конфигурацию, ANN-индекса нет.
/// Это осознанно (так же у первоисточника), но означает, что стоимость
/// запроса линейна по корпусу, и на порядок больших конфигурациях
/// понадобится либо отбор кандидатов, либо ANN.
///
/// # Кэш корпуса
///
/// Перебор полный, и до 01.10.2026 каждый вызов заново читал все векторы
/// из SQLite: на корпусе в 580 тысяч это 1,4 из 1,8 с одного `find`.
/// Если кэш включён ([`set_cache_limit`]) и соединение открыто на файл,
/// корпус читается один раз и дальше берётся из памяти. Выдача от кэша
/// не зависит — это проверяется тестом, а не обещается.
pub fn search(
    conn: &Connection,
    query: &str,
    kind: Option<&str>,
    limit: usize,
) -> Result<Vec<Hit>> {
    if let Some(корпус) = cache::получить(conn)? {
        return search_in(conn, &корпус, query, kind, limit);
    }
    search_streaming(conn, query, kind, limit)
}

/// Вектор запроса: та же токенизация и те же веса, что при сборке.
/// `None` — запрос не дал ни одного ненулевого вектора.
fn query_vector(conn: &Connection, dict: &Dictionary, query: &str) -> Result<Option<Vector>> {
    let tokens = gyrfalcon_parser::tokens::tokenize(query);
    if tokens.is_empty() {
        return Ok(None);
    }
    let mut idf_of = HashMap::new();
    {
        let mut st = conn.prepare("SELECT idf FROM semantic_tokens WHERE token = ?1")?;
        for t in &tokens {
            let v: f32 = st.query_row([t], |r| r.get(0)).unwrap_or(1.0);
            idf_of.insert(t.clone(), v);
        }
    }
    // Запрос считается тем же правилом, что и имена при сборке (Р-014):
    // пути не смешиваются, иначе вектор запроса окажется в геометрии,
    // которой нет ни у одного вектора корпуса.
    let есть_словарные = tokens
        .iter()
        .any(|t| dict.vector(t).1 == VectorSource::Dictionary);
    let mut acc = vec![0f32; DIM];
    for t in &tokens {
        let (v, src) = dict.vector(t);
        if src != VectorSource::Dictionary && есть_словарные {
            continue;
        }
        let w = idf_of.get(t).copied().unwrap_or(1.0);
        for (k, slot) in acc.iter_mut().enumerate() {
            *slot += v[k] as f32 * w;
        }
    }
    let norm = acc.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= f32::EPSILON {
        return Ok(None);
    }
    let mut q = [0i8; DIM];
    for (k, slot) in q.iter_mut().enumerate() {
        *slot = (acc[k] / norm * 127.0).round().clamp(-127.0, 127.0) as i8;
    }
    Ok(Some(q))
}

/// Обрезать по лимиту и нормировать по выдаче. Общая часть обоих путей.
fn finish(mut hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    hits.truncate(limit);
    // Нормировка по выдаче — тот самый урок 10.08.2026. Она нужна не здесь
    // (одиночный сигнал ранжируется и без неё), а на входе в общую формулу
    // с лексикой: без приведения к общему разбросу косинус в сумме исчезает.
    // Поэтому `raw` сохраняется рядом — иначе после нормировки «лучший из
    // мусора» и «точное попадание» выглядят одинаково.
    let mut scores: Vec<f32> = hits.iter().map(|h| h.raw).collect();
    rank_normalized(&mut scores);
    for (h, s) in hits.iter_mut().zip(scores) {
        h.score = s;
    }
    hits
}

/// Прежний путь без кэша: векторы читаются из SQLite потоком.
///
/// Остаётся для CLI и встраивающих приложений, где кэш выключен: там
/// разовый запрос не должен закреплять за процессом сотни мегабайт.
fn search_streaming(
    conn: &Connection,
    query: &str,
    kind: Option<&str>,
    limit: usize,
) -> Result<Vec<Hit>> {
    let dict = Dictionary::load(conn)?;
    let Some(q) = query_vector(conn, &dict, query)? else {
        return Ok(Vec::new());
    };

    let sql = match kind {
        Some(_) => "SELECT kind, ref_id, name, vector FROM semantic_vectors WHERE kind = ?1",
        None => "SELECT kind, ref_id, name, vector FROM semantic_vectors",
    };
    let mut st = conn.prepare(sql)?;
    let map = |r: &rusqlite::Row| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Vec<u8>>(3)?,
        ))
    };
    let rows: Box<dyn Iterator<Item = rusqlite::Result<_>>> = match kind {
        Some(k) => Box::new(st.query_map([k], map)?),
        None => Box::new(st.query_map([], map)?),
    };

    let mut hits: Vec<Hit> = Vec::new();
    for row in rows {
        let (kind, ref_id, name, blob) = row?;
        if blob.len() != DIM {
            continue;
        }
        let mut v = [0i8; DIM];
        for (i, b) in blob.iter().enumerate() {
            v[i] = *b as i8;
        }
        let raw = cosine(&q, &v);
        hits.push(Hit {
            kind,
            ref_id,
            name,
            score: raw,
            raw,
        });
    }

    hits.sort_by(|a, b| {
        b.raw
            .partial_cmp(&a.raw)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(finish(hits, limit))
}

/// Корпус индекса в памяти: словарь и все векторы одним плоским буфером.
///
/// Плоский `Vec<i8>`, а не `Vec<[i8; DIM]>` с именами рядом: перебор идёт
/// по памяти подряд, и норма каждого вектора посчитана заранее — при
/// поиске остаётся одно скалярное произведение на строку.
pub struct Corpus {
    dict: Dictionary,
    /// Различные значения `kind` (их два-три); у строки — номер в этом списке.
    kinds: Vec<String>,
    kind_of: Vec<u8>,
    ref_ids: Vec<i64>,
    names: Vec<String>,
    vectors: Vec<i8>,
    /// Квадрат нормы каждого вектора — целым, как его считает [`cosine`].
    norms: Vec<i32>,
}

impl Corpus {
    /// Прочитать корпус в том же порядке строк, что и потоковый путь.
    pub fn load(conn: &Connection) -> Result<Self> {
        let dict = Dictionary::load(conn)?;
        let mut c = Corpus {
            dict,
            kinds: Vec::new(),
            kind_of: Vec::new(),
            ref_ids: Vec::new(),
            names: Vec::new(),
            vectors: Vec::new(),
            norms: Vec::new(),
        };
        // Нет таблицы — та же ошибка prepare, что у потокового пути: кэш не
        // должен превращать отказ в пустую выдачу.
        let mut st = conn.prepare("SELECT kind, ref_id, name, vector FROM semantic_vectors")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let blob = r.get_ref(3)?.as_blob().map_err(rusqlite::Error::from)?;
            if blob.len() != DIM {
                continue; // то же правило, что у потокового пути: порча не вектор
            }
            let kind: String = r.get(0)?;
            let k = match c.kinds.iter().position(|x| *x == kind) {
                Some(i) => i,
                None => {
                    c.kinds.push(kind);
                    c.kinds.len() - 1
                }
            };
            c.kind_of.push(k as u8);
            c.ref_ids.push(r.get(1)?);
            c.names.push(r.get(2)?);
            let начало = c.vectors.len();
            c.vectors.extend(blob.iter().map(|b| *b as i8));
            c.norms
                .push(dot(&c.vectors[начало..], &c.vectors[начало..]));
        }
        c.vectors.shrink_to_fit();
        Ok(c)
    }

    /// Строк в корпусе.
    pub fn len(&self) -> usize {
        self.ref_ids.len()
    }

    /// Пуст ли корпус.
    pub fn is_empty(&self) -> bool {
        self.ref_ids.is_empty()
    }

    /// Сколько памяти держит корпус, байт (оценка снизу: без служебных
    /// заголовков аллокатора). Ею меряется потолок кэша.
    pub fn bytes(&self) -> usize {
        self.vectors.len()
            + self.norms.len() * 4
            + self.ref_ids.len() * 8
            + self.kind_of.len()
            + self.names.iter().map(|n| n.len() + 24).sum::<usize>()
            + self.dict.len() * (DIM + 48)
    }
}

fn dot(a: &[i8], b: &[i8]) -> i32 {
    a.iter().zip(b).map(|(x, y)| *x as i32 * *y as i32).sum()
}

/// Поиск по корпусу в памяти. Результат совпадает с потоковым путём
/// бит в бит: тот же косинус, тот же порядок при равных оценках.
pub fn search_in(
    conn: &Connection,
    корпус: &Corpus,
    query: &str,
    kind: Option<&str>,
    limit: usize,
) -> Result<Vec<Hit>> {
    let Some(q) = query_vector(conn, &корпус.dict, query)? else {
        return Ok(Vec::new());
    };
    let qn = dot(&q, &q);
    let фильтр: Option<u8> = match kind {
        Some(k) => match корпус.kinds.iter().position(|x| x == k) {
            Some(i) => Some(i as u8),
            None => return Ok(finish(Vec::new(), limit)),
        },
        None => None,
    };

    let mut оценки: Vec<(f32, usize)> = Vec::with_capacity(корпус.len());
    for i in 0..корпус.len() {
        if let Some(f) = фильтр {
            if корпус.kind_of[i] != f {
                continue;
            }
        }
        let v = &корпус.vectors[i * DIM..(i + 1) * DIM];
        let nb = корпус.norms[i];
        // Ровно формула `cosine`: те же целые, те же преобразования в f32.
        let raw = if qn == 0 || nb == 0 {
            0.0
        } else {
            let c = dot(&q, v) as f32 / ((qn as f32).sqrt() * (nb as f32).sqrt());
            ((c + 1.0) / 2.0).clamp(0.0, 1.0)
        };
        оценки.push((raw, i));
    }

    // Порядок потокового пути — устойчивая сортировка по убыванию, то есть
    // при равенстве раньше идёт строка, прочитанная раньше. Номер строки
    // в сравнении воспроизводит это без сортировки всех 580 тысяч.
    let порядок = |a: &(f32, usize), b: &(f32, usize)| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    };
    if limit < оценки.len() {
        оценки.select_nth_unstable_by(limit, порядок);
        оценки.truncate(limit);
    }
    оценки.sort_by(порядок);

    let hits = оценки
        .into_iter()
        .map(|(raw, i)| Hit {
            kind: корпус.kinds[корпус.kind_of[i] as usize].clone(),
            ref_id: корпус.ref_ids[i],
            name: корпус.names[i].clone(),
            score: raw,
            raw,
        })
        .collect();
    Ok(finish(hits, limit))
}

/// Кэш корпусов на процесс: ключ — путь файла индекса.
///
/// # Почему на процесс, а не на соединение
///
/// Сервер закрывает и заново открывает соединение на время автодосборки,
/// а встраивающие приложения держат свои соединения. Привязка к пути
/// и отпечатку файла переживает и то и другое.
///
/// # Когда кэш перестаёт быть правдой
///
/// Отпечаток — размер и время изменения файла индекса и его `-wal`.
/// Любая запись (досборка, полная пересборка поверх) меняет его, и корпус
/// перечитывается при следующем вызове. Сервер вдобавок сбрасывает
/// корпус явно после досборки ([`cache::forget`]) — не полагаясь на
/// разрешение времени файловой системы.
pub mod cache {
    use super::Corpus;
    use crate::Result;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::SystemTime;

    type Отпечаток = Vec<(u64, Option<SystemTime>)>;

    struct Запись {
        корпус: Arc<Corpus>,
        отпечаток: Отпечаток,
        байт: usize,
        обращение: u64,
    }

    #[derive(Default)]
    struct Кэш {
        потолок: usize,
        записи: HashMap<PathBuf, Запись>,
        счётчик: u64,
    }

    fn кэш() -> &'static Mutex<Кэш> {
        static К: OnceLock<Mutex<Кэш>> = OnceLock::new();
        К.get_or_init(|| Mutex::new(Кэш::default()))
    }

    /// Задать потолок памяти под корпуса, байт. 0 — кэш выключен
    /// (умолчание: включает тот, кто знает, что процесс живёт долго).
    pub fn set_limit(байт: usize) {
        let mut к = кэш().lock().unwrap();
        к.потолок = байт;
        вытеснить(&mut к, 0);
    }

    /// Сбросить корпус индекса — после записи в него.
    pub fn forget(db: &Path) {
        let mut к = кэш().lock().unwrap();
        к.записи.remove(db);
    }

    /// Что сейчас в кэше: путь, строк, байт — для отчёта сервера.
    pub fn stats() -> Vec<(PathBuf, usize, usize)> {
        let к = кэш().lock().unwrap();
        к.записи
            .iter()
            .map(|(p, з)| (p.clone(), з.корпус.len(), з.байт))
            .collect()
    }

    fn отпечаток(db: &Path) -> Отпечаток {
        let wal = PathBuf::from(format!("{}-wal", db.display()));
        [db.to_path_buf(), wal]
            .iter()
            .map(|p| match std::fs::metadata(p) {
                Ok(m) => (m.len(), m.modified().ok()),
                Err(_) => (0, None),
            })
            .collect()
    }

    fn вытеснить(к: &mut Кэш, нужно: usize) {
        loop {
            let занято: usize = к.записи.values().map(|з| з.байт).sum();
            if занято + нужно <= к.потолок || к.записи.is_empty() {
                return;
            }
            let старейший = к
                .записи
                .iter()
                .min_by_key(|(_, з)| з.обращение)
                .map(|(p, _)| p.clone())
                .expect("непусто");
            к.записи.remove(&старейший);
        }
    }

    /// Корпус для соединения или `None`, если кэш выключен, база не в файле
    /// (`:memory:`) или корпус больше потолка целиком.
    pub(super) fn получить(conn: &Connection) -> Result<Option<Arc<Corpus>>> {
        let Some(путь) = conn.path().filter(|p| !p.is_empty()).map(PathBuf::from) else {
            return Ok(None);
        };
        let отп = отпечаток(&путь);
        {
            let mut к = кэш().lock().unwrap();
            if к.потолок == 0 {
                return Ok(None);
            }
            к.счётчик += 1;
            let n = к.счётчик;
            if let Some(з) = к.записи.get_mut(&путь) {
                if з.отпечаток == отп {
                    з.обращение = n;
                    return Ok(Some(з.корпус.clone()));
                }
            }
            к.записи.remove(&путь);
        }
        // Читаем без замка: чтение корпуса — секунды, и держать замок
        // ради него значит остановить поиск по другим индексам.
        let корпус = Arc::new(Corpus::load(conn)?);
        let байт = корпус.bytes();
        let mut к = кэш().lock().unwrap();
        if байт > к.потолок {
            // Не помещается даже один — отвечаем по нему, но не держим.
            return Ok(Some(корпус));
        }
        вытеснить(&mut к, байт);
        к.счётчик += 1;
        let обращение = к.счётчик;
        к.записи.insert(
            путь,
            Запись {
                корпус: корпус.clone(),
                отпечаток: отп,
                байт,
                обращение,
            },
        );
        Ok(Some(корпус))
    }
}

/// Задать потолок кэша корпусов, мегабайт. 0 — выключить. См. [`cache`].
pub fn set_cache_limit(мб: usize) {
    cache::set_limit(мб * 1024 * 1024);
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::sync::Arc;

    /// Индекс-файл с векторами: часть строк — точные дубли (равные оценки),
    /// два вида, одна строка с испорченным блобом.
    fn база(имя: &str) -> (std::path::PathBuf, Connection) {
        let p = std::env::temp_dir().join(format!("gf-semcache-{имя}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE semantic_tokens(token TEXT PRIMARY KEY, idf REAL);
             CREATE TABLE semantic_vectors(id INTEGER PRIMARY KEY, kind TEXT, ref_id INTEGER,
                                           name TEXT, vector BLOB);",
        )
        .unwrap();
        let слова = [
            "заказ",
            "клиента",
            "проведение",
            "себестоимость",
            "номенклатура",
        ];
        let mut n = 0i64;
        for повтор in 0..3 {
            for (i, слово) in слова.iter().enumerate() {
                let v = random_indexing(слово);
                let blob: Vec<u8> = v.iter().map(|x| *x as u8).collect();
                let вид = if i % 2 == 0 { "method" } else { "object" };
                n += 1;
                c.execute(
                    "INSERT INTO semantic_vectors(kind, ref_id, name, vector) VALUES (?1,?2,?3,?4)",
                    rusqlite::params![вид, n, format!("{слово}{повтор}"), blob],
                )
                .unwrap();
            }
        }
        c.execute(
            "INSERT INTO semantic_vectors(kind, ref_id, name, vector) VALUES ('method', 999, 'порча', x'0102')",
            [],
        )
        .unwrap();
        c.execute("INSERT INTO semantic_tokens VALUES ('заказ', 2.5)", [])
            .unwrap();
        (p, c)
    }

    fn одинаковы(ждали: &[Hit], вышло: &[Hit]) {
        assert_eq!(ждали.len(), вышло.len());
        for (x, y) in ждали.iter().zip(вышло) {
            assert_eq!((&x.kind, x.ref_id, &x.name), (&y.kind, y.ref_id, &y.name));
            assert_eq!(x.raw.to_bits(), y.raw.to_bits(), "raw у {}", x.name);
            assert_eq!(x.score.to_bits(), y.score.to_bits(), "score у {}", x.name);
        }
    }

    #[test]
    fn корпус_в_памяти_даёт_ту_же_выдачу_бит_в_бит() {
        let (p, c) = база("равенство");
        let корпус = Corpus::load(&c).unwrap();
        assert_eq!(корпус.len(), 15, "испорченный блоб в корпус не входит");
        for запрос in [
            "заказ",
            "заказ клиента",
            "себестоимость номенклатуры",
            "ъъъ",
        ] {
            for вид in [None, Some("method"), Some("object"), Some("нет-такого")] {
                for лимит in [1, 3, 7, 100] {
                    let ждали = search_streaming(&c, запрос, вид, лимит).unwrap();
                    let вышло = search_in(&c, &корпус, запрос, вид, лимит).unwrap();
                    одинаковы(&ждали, &вышло);
                }
            }
        }
        drop(c);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn кэш_сбрасывается_при_записи_в_индекс() {
        let (p, c) = база("сброс");
        cache::set_limit(64 * 1024 * 1024);
        let первый = cache::получить(&c).unwrap().expect("кэш включён");
        let второй = cache::получить(&c).unwrap().unwrap();
        assert!(
            Arc::ptr_eq(&первый, &второй),
            "повторный вызов берёт из памяти"
        );

        let v: Vec<u8> = random_indexing("новое").iter().map(|x| *x as u8).collect();
        c.execute(
            "INSERT INTO semantic_vectors(kind, ref_id, name, vector) VALUES ('method', 1000, 'новое', ?1)",
            [v],
        )
        .unwrap();
        let после = cache::получить(&c).unwrap().unwrap();
        assert_eq!(
            после.len(),
            первый.len() + 1,
            "запись в файл видна без перезапуска"
        );

        cache::forget(&p);
        let после_сброса = cache::получить(&c).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&после, &после_сброса));

        cache::forget(&p);
        drop(c);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn база_в_памяти_не_кэшируется() {
        let c = Connection::open_in_memory().unwrap();
        cache::set_limit(64 * 1024 * 1024);
        assert!(cache::получить(&c).unwrap().is_none());
    }
}
