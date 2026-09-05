//! Цикл MCP-сервера: stdin → обработка → stdout.
//!
//! # Транспорт
//!
//! Одна строка = одно сообщение JSON-RPC. Это транспорт stdio, каким его
//! ожидают клиенты MCP.
//!
//! # Почему stdout свят
//!
//! В stdout идёт **только** JSON-RPC. Любая диагностика — в stderr. Одна
//! отладочная строка, напечатанная в stdout, делает сервер неработающим для
//! клиента, и выглядит это как «сервер не отвечает», а не как опечатка.

use crate::proto::{self, ErrorCode, Request};
use crate::tools::{self, Profile};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;

pub struct Server {
    /// Откуда брать индексы: один файл или каталог проектов (Р-004).
    источник: crate::registry::Источник,
    profile: Profile,
    /// Соединения по проектам: открываются лениво, живут до конца сессии.
    ///
    /// Кэш, а не пул: у SQLite на чтение соединение дешёвое, но открывать
    /// его на каждый вызов значит платить разбором схемы за каждый вопрос.
    соединения: std::collections::HashMap<String, Connection>,
    /// Сторожа свежести — свой на каждый проект (веха 7).
    ///
    /// Поднимаются лениво: сервер обязан отвечать на `initialize` даже при
    /// недоступном индексе, а сторож — это уже работа с ним. Свой на проект
    /// потому, что у каждого индекса свой каталог исходников и своё время
    /// сборки; один сторож на всех отвечал бы про чужую конфигурацию.
    сторожа: std::collections::HashMap<String, crate::freshness_guard::FreshnessGuard>,
    /// Автодосборка по проектам — своя на каждый, как и сторож.
    досборка: std::collections::HashMap<String, crate::autoupdate::Автодосборка>,
    /// Включена ли автодосборка (флаг `--auto-update`).
    авто: bool,
    /// Когда последний раз пробовали догнать проект (unix, мс).
    ///
    /// Троттлинг. У образца досборку запускает фоновый наблюдатель по
    /// таймеру, у нас фонового потока нет — зато есть готовый повод
    /// проверить: агент задал вопрос. Интервал образца применяется здесь
    /// как «не чаще, чем», иначе досборка шла бы на каждый вызов.
    последняя_проверка: std::collections::HashMap<String, u64>,
}

impl Server {
    pub fn new(источник: crate::registry::Источник, profile: Profile) -> Self {
        Self {
            источник,
            profile,
            соединения: std::collections::HashMap::new(),
            сторожа: std::collections::HashMap::new(),
            досборка: std::collections::HashMap::new(),
            авто: false,
            последняя_проверка: std::collections::HashMap::new(),
        }
    }

    /// Включить автодосборку индекса по изменившимся `.bsl`.
    pub fn с_автодосборкой(mut self, включена: bool) -> Self {
        self.авто = включена;
        self
    }

    /// Соединение и сторож для проекта, названного в аргументах вызова.
    ///
    /// Возвращает ключ проекта, чтобы вызывающий знал, к какому индексу
    /// он обратился: при `Один` имя можно не называть, и без ключа
    /// нельзя было бы найти его сторожа.
    ///
    /// Открывается лениво и только на чтение. Лениво — чтобы сервер
    /// поднимался и отвечал `initialize` даже при недоступном индексе:
    /// клиент должен получить внятный отказ на вызове инструмента,
    /// а не молчание при старте.
    fn открыть(&mut self, project: Option<&str>) -> Result<String, String> {
        let путь = self.источник.путь(project).map_err(|e| e.to_string())?;
        let ключ = путь.to_string_lossy().to_string();
        if !self.соединения.contains_key(&ключ) {
            let c = crate::sql::open_readonly(&путь).map_err(|e| {
                format!(
                    "индекс {} недоступен: {e}. Соберите его:                      gyrfalcon build <путь> --out <файл>",
                    путь.display()
                )
            })?;
            self.соединения.insert(ключ.clone(), c);
            self.сторожа.insert(
                ключ.clone(),
                crate::freshness_guard::FreshnessGuard::запустить(&путь),
            );
            let корень = gyrfalcon_index::build::info(&путь)
                .map(|i| std::path::PathBuf::from(i.source_path))
                .unwrap_or_default();
            self.досборка.insert(
                ключ.clone(),
                crate::autoupdate::Автодосборка::новая(&путь, &корень, self.авто),
            );
        }
        Ok(ключ)
    }

    /// Обработать одно сообщение. `None` — отвечать не нужно (уведомление).
    pub fn handle(&mut self, line: &str) -> Option<Value> {
        let req: Request = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return Some(proto::err(
                    Value::Null,
                    ErrorCode::ParseError,
                    format!("не разобрал JSON: {e}"),
                ))
            }
        };

        // Уведомления ответа не требуют — на них молчим (это по протоколу,
        // а не потому, что нечего сказать).
        let id = req.id.clone()?;

        match req.method.as_str() {
            "initialize" => {
                // Версию диктует клиент, а не транспорт: по stdio приходят
                // 2024-11-05, по HTTP — 2025-03-26 и новее.
                let протокол = proto::согласовать_версию(
                    req.params.get("protocolVersion").and_then(Value::as_str),
                );
                Some(proto::ok(
                    id,
                    proto::initialize_result("gyrfalcon", env!("CARGO_PKG_VERSION"), протокол),
                ))
            }
            "ping" => Some(proto::ok(id, json!({}))),
            "tools/list" => {
                let список: Vec<Value> = tools::list(self.profile)
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": (t.schema)()
                        })
                    })
                    .collect();
                Some(proto::ok(id, json!({"tools": список})))
            }
            "tools/call" => {
                let имя = req
                    .params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if имя.is_empty() {
                    return Some(proto::err(
                        id,
                        ErrorCode::InvalidParams,
                        "не указано имя инструмента",
                    ));
                }
                let args = req
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let profile = self.profile;

                // `projects` отвечает про сам сервер, а не про конкретный
                // индекс: ему проект не нужен и открывать ничего не надо.
                if имя == "delete_project" {
                    let итог = self.удалить_проект(&args);
                    return Some(match итог {
                        Ok(v) => proto::ok(
                            id,
                            proto::tool_result(
                                serde_json::to_string(&v).unwrap_or_else(|e| e.to_string()),
                                false,
                            ),
                        ),
                        Err(e) => proto::ok(id, proto::tool_result(e, true)),
                    });
                }

                if имя == "list_projects" {
                    return Some(proto::ok(
                        id,
                        proto::tool_result(
                            serde_json::to_string(&self.перечислить())
                                .unwrap_or_else(|e| e.to_string()),
                            false,
                        ),
                    ));
                }

                let итог = self.выполнить(profile, &имя, &args);
                // Ошибка инструмента — это успешный ответ с isError, а не
                // ошибка протокола: клиент должен отличать «не вышло» от
                // «сервер сломан».
                Some(match итог {
                    Ok(v) => proto::ok(
                        id,
                        proto::tool_result(
                            serde_json::to_string(&v).unwrap_or_else(|e| e.to_string()),
                            false,
                        ),
                    ),
                    Err(e) => proto::ok(id, proto::tool_result(e, true)),
                })
            }
            other => Some(proto::err(
                id,
                ErrorCode::MethodNotFound,
                format!("метод не поддерживается: {other}"),
            )),
        }
    }

    /// Выполнить инструмент по одному или нескольким проектам.
    ///
    /// `project` принимает три формы (устройство взято у образца,
    /// `target_projects`): строку, массив строк и `"*"` — все проекты.
    ///
    /// # Почему ответ по нескольким выглядит иначе
    ///
    /// Он не склеивается в одну таблицу. Строки разных конфигураций
    /// одинаковы на вид и означают разное: `Контрагенты` из ЕРП.УХ и
    /// `Контрагенты` из БП — разные объекты разных баз. Склеенная таблица
    /// это скрывает, и вывод о «дубле» или «единственном совпадении»
    /// делается по перемешанным данным. Поэтому ответ — карта
    /// «проект → его ответ», и принадлежность каждой строки видна.
    /// Догнать индекс проекта по накопившимся правкам `.bsl`.
    ///
    /// Вызывается перед выполнением инструмента и почти всегда не делает
    /// ничего: очередь пуста либо интервал ещё не вышел.
    ///
    /// # Почему соединение закрывается
    ///
    /// Инструменты держат соединение ТОЛЬКО НА ЧТЕНИЕ, а инкремент пишет.
    /// Держать рядом второе, пишущее, значило бы получить `database is
    /// locked` в самый неудачный момент — поэтому читающее закрывается
    /// на время записи и открывается заново. Цена — разбор схемы один раз
    /// на досборку, то есть примерно никогда.
    fn догнать(&mut self, ключ: &str) {
        if !self.авто {
            return;
        }
        let Some(досборка) = self.досборка.get(ключ) else {
            return;
        };
        if !досборка.включена() {
            return;
        }
        let Some(страж) = self.сторожа.get(ключ) else {
            return;
        };
        let очередь = страж.очередь();
        if очередь.is_empty() {
            return;
        }

        // Троттлинг по формуле образца: интервал растёт с размером корпуса.
        // Размер берём из числа модулей индекса — оно уже посчитано.
        let сейчас = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let файлов = self
            .соединения
            .get(ключ)
            .and_then(|c| {
                c.query_row("SELECT COUNT(*) FROM modules", [], |r| r.get::<_, i64>(0))
                    .ok()
            })
            .unwrap_or(0) as u64;
        let интервал = crate::autoupdate::интервал_мс(файлов);
        if let Some(прошлая) = self.последняя_проверка.get(ключ) {
            if сейчас.saturating_sub(*прошлая) < интервал {
                return;
            }
        }
        self.последняя_проверка.insert(ключ.to_string(), сейчас);

        // Закрыть читающее соединение на время записи (см. выше).
        let путь = std::path::PathBuf::from(ключ);
        self.соединения.remove(ключ);
        let итог = досборка.догнать(&очередь);
        if let Ok(c) = crate::sql::open_readonly(&путь) {
            self.соединения.insert(ключ.to_string(), c);
        }

        // Базовую линию сдвигаем ТОЛЬКО при успехе — правило образца (#937).
        // Отказ по метаданным и сбой оставляют очередь нетронутой: отметка
        // сторожа продолжает говорить, что индекс отстал, и это правда.
        if итог.is_ok() {
            if let Some(страж) = self.сторожа.get(ключ) {
                страж.признать_догнанным(&очередь);
            }
        }
    }

    fn выполнить(
        &mut self,
        profile: Profile,
        имя: &str,
        args: &Value,
    ) -> Result<Value, String> {
        let mut цели = разобрать_цели(args);

        // Звёздочка разворачивается ДО выбора ветки. Иначе `project: "*"`
        // — одна строка, то есть «одна цель», — уходила бы в поиск проекта
        // с именем `*`, которого нет. Поймано живой пробой: отказ
        // «проект '*' не найден» при трёх доступных.
        if цели.iter().any(|ц| ц == "*") {
            цели = self.источник.имена_всех();
        }

        // Одна цель (или ни одной) — прежний путь, ответ без обёртки.
        if цели.len() <= 1 {
            let один = цели.first().map(String::as_str);
            let ключ = self.открыть(один)?;
            // Догнать индекс ДО ответа, а не после: смысл автодосборки в том,
            // чтобы агент получил ответ по свежему индексу, а не узнал задним
            // числом, что отвечали ему по вчерашнему.
            self.догнать(&ключ);
            let c = self.соединения.get(&ключ).expect("только что открыт");
            let r = tools::call(c, profile, имя, args);
            // Отметка свежести приписывается к УСПЕШНОМУ ответу (веха 7)
            // и берётся у сторожа ЭТОГО проекта. К отказу — нет: у отказа
            // своя причина, и две беды в одном тексте читаются как одна.
            return match (r, self.сторожа.get(&ключ)) {
                (Ok(v), Some(страж)) => Ok(страж.пометить(v)),
                (иное, _) => иное,
            };
        }

        let список = self.источник.пути(&цели).map_err(|e| e.to_string())?;
        let mut ответы = serde_json::Map::new();
        let mut удачных = 0usize;
        for (проект, _) in &список {
            // Отказ по ОДНОМУ проекту не отменяет остальные: индекс мог быть
            // испорчен, и «всё упало» скрыло бы работающие ответы. Но и молча
            // пропустить нельзя — он попадает в ответ как ошибка этой строки.
            let значение = match self.открыть(Some(проект)) {
                Ok(ключ) => {
                    let c = self.соединения.get(&ключ).expect("только что открыт");
                    match tools::call(c, profile, имя, args) {
                        Ok(v) => {
                            удачных += 1;
                            match self.сторожа.get(&ключ) {
                                Some(страж) => страж.пометить(v),
                                None => v,
                            }
                        }
                        Err(e) => json!({"error": e}),
                    }
                }
                Err(e) => json!({"error": e}),
            };
            ответы.insert(проект.clone(), значение);
        }
        Ok(json!({
            "by_project": Value::Object(ответы),
            "projects_queried": список.len(),
            "projects_answered": удачных,
            "note": "Ответы разделены по конфигурациям намеренно: одноимённые                      объекты разных баз — разные объекты, и склеенная таблица                      скрыла бы, чей это результат"
        }))
    }

    /// Удалить проект: закрыть его, снять сторожа, убрать файл.
    ///
    /// # Почему это инструмент, а не `rm`
    ///
    /// Первая редакция обходилась без него с доводом «проект — это файл,
    /// удаление — это `rm`». Довод оказался неверным, и проверка заняла
    /// одну пробу: пока сервер работает, `rm` на Windows **не проходит
    /// вовсе** — `WinError 32`, файл занят процессом. Чтобы удалить
    /// конфигурацию, пришлось бы гасить сервер, то есть ронять работу
    /// всех агентов ради одной базы.
    ///
    /// Порядок здесь тот же, что у образца (так же устроено у индексаторов общего назначения), и
    /// каждый шаг обязателен:
    ///
    /// 1. **закрыть соединение** — иначе файл занят и удаление откажет;
    /// 2. **снять сторожа** — наблюдатель за исчезнувшим каталогом это
    ///    поток, следящий за пустотой, и живёт он до конца сессии;
    /// 3. **удалить файл** вместе со спутниками SQLite;
    /// 4. **назвать исход словом**, а не «готово»: удалён, не найден,
    ///    не удалось — с причиной.
    ///
    /// Чего здесь НЕТ по сравнению с образцом: блокировки конвейера
    /// индексации. У них сборка идёт в том же процессе и её надо дождаться;
    /// у нас `build` — отдельная команда, и параллельная сборка пишет
    /// в свой файл.
    fn удалить_проект(&mut self, args: &Value) -> Result<Value, String> {
        let имя = args
            .get("project")
            .and_then(Value::as_str)
            .ok_or("нужен параметр project: удалять без имени нечего")?;

        let путь = self.источник.путь(Some(имя)).map_err(|e| e.to_string())?;
        let ключ = путь.to_string_lossy().to_string();

        // 1-2. Закрыть и снять сторожа. `remove` возвращает значение,
        // которое тут же уничтожается — это и есть закрытие: у `Connection`
        // и у наблюдателя `notify` освобождение происходит в `Drop`.
        let было_открыто = self.соединения.remove(&ключ).is_some();
        self.сторожа.remove(&ключ);

        // 3. Удалить файл и спутники. `-wal`/`-shm` у нас не появляются
        // (индекс открывается только на чтение), но сборка идёт на запись,
        // и оборванная сборка их оставляет. Чистим — иначе они переживут
        // свой индекс и будут занимать место без всякой пользы.
        // Ветка недостижима на обычном пути: реестр отсекает несуществующий
        // проект раньше, отказом «не найден» с перечнем доступных (проверено
        // живьём — повторное удаление даёт именно его). Оставлена для гонки:
        // файл могли удалить между разрешением имени и этой строкой.
        if !путь.is_file() {
            return Ok(json!({
                "project": имя,
                "status": "not_found",
                "note": "файла индекса нет — удалять нечего"
            }));
        }
        if let Err(e) = std::fs::remove_file(&путь) {
            return Ok(json!({
                "project": имя,
                "status": "delete_failed",
                "error": e.to_string(),
                "note": "индекс закрыт сервером, но файл не удалён.                          Обычная причина — файл открыт другим процессом                          (вторым сервером, сборкой, просмотрщиком SQLite)"
            }));
        }
        let mut спутники = 0;
        for суффикс in ["-wal", "-shm"] {
            let спутник = PathBuf::from(format!("{}{суффикс}", путь.display()));
            if спутник.is_file() && std::fs::remove_file(&спутник).is_ok() {
                спутники += 1;
            }
        }

        Ok(json!({
            "project": имя,
            "status": "deleted",
            "path": путь.display().to_string(),
            "was_open": было_открыто,
            "companions_removed": спутники,
            "note": "удалён ИНДЕКС, а не исходники конфигурации.                      Собрать заново: gyrfalcon build <путь> --out <файл>"
        }))
    }

    /// Ответ инструмента `projects`.
    ///
    /// Отдаёт и состояние сторожа свежести по каждому открытому проекту.
    /// Причина в вопросе, на который иначе нечем ответить: «свежий индекс
    /// молчит» — значит молчание надо уметь ПРОВЕРИТЬ. Без этого оно
    /// означает и «всё хорошо», и «сторож не встал, и ты не узнаешь».
    fn перечислить(&self) -> Value {
        let строки: Vec<Value> = self
            .источник
            .перечислить()
            .into_iter()
            .map(|p| {
                let ключ = p.индекс.to_string_lossy().to_string();
                // Проект, которого ещё не спрашивали в этой сессии, сторожа
                // не имеет — и это называется прямо, а не выдаётся молчанием
                // за «всё хорошо».
                let mut свежесть = match self.сторожа.get(&ключ) {
                    Some(страж) => страж.состояние(),
                    None => json!({"checked": "не открывался в этой сессии"}),
                };
                // Автодосборка — часть ответа на «откуда я знаю, что всё
                // нормально»: выключенная она говорит об этом прямо, иначе
                // «ничего не досбиралось» читалось бы как «нечего было».
                //
                // Проект, не открытый в этой сессии, своей `Автодосборки`
                // ещё не имеет — `list_projects` проектов не открывает.
                // Но флаг сервера известен всегда, и отвечать `null` на
                // вопрос «включена ли она» нельзя: спрашивают именно затем,
                // чтобы узнать состояние, а `null` читается как «неизвестно»
                // там, где известно. Поймано живой пробой 31.08.2026 сразу
                // после включения флага в конфигурации.
                if let Some(o) = свежесть.as_object_mut() {
                    match self.досборка.get(&ключ) {
                        Some(д) => {
                            if let Some(состояние) = д.состояние().as_object() {
                                for (k, v) in состояние {
                                    o.insert(k.clone(), v.clone());
                                }
                            }
                        }
                        None => {
                            o.insert(
                                "auto_update".into(),
                                json!(if self.авто {
                                    "включена"
                                } else {
                                    "выключена"
                                }),
                            );
                        }
                    }
                }
                json!([p.имя, p.исходники, p.модулей, p.методов, p.собран, свежесть])
            })
            .collect();
        json!({
            "columns": ["project", "source_path", "modules", "methods", "built_at", "freshness"],
            "rows": строки,
            "count": строки.len(),
            "note": "built_at — секунды эпохи. Нули в счётчиках значат, что файл                      индекса есть, но не читается — это видно намеренно, чтобы                      испорченный индекс не пропадал из списка молча.                      freshness отвечает на вопрос «а откуда я знаю, что всё                      нормально»: свежий индекс в ответах инструментов МОЛЧИТ,                      и проверить это молчание можно только здесь"
        })
    }

    pub fn описание_источника(&self) -> String {
        match &self.источник {
            crate::registry::Источник::Один { путь, .. } => {
                путь.display().to_string()
            }
            crate::registry::Источник::Каталог(d) => {
                format!(
                    "каталог {} ({} проектов)",
                    d.display(),
                    self.источник.перечислить().len()
                )
            }
        }
    }

    /// Читать stdin до конца, отвечая в stdout.
    pub fn run(&mut self) -> std::io::Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        eprintln!(
            "gyrfalcon {} — MCP на stdio, индекс {}, профиль {:?}",
            env!("CARGO_PKG_VERSION"),
            self.описание_источника(),
            self.profile
        );
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(ответ) = self.handle(&line) {
                serde_json::to_writer(&mut stdout, &ответ)?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
        }
        Ok(())
    }
}

/// Извлечь список проектов из аргументов вызова.
///
/// Пусто — проект не назван (реестр решит, отказ это или единственный
/// индекс). Одно имя — строка. Несколько — массив. `"*"` передаётся дальше
/// как есть: разворачивает его реестр, который один знает состав.
fn разобрать_цели(args: &Value) -> Vec<String> {
    match args.get("project") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn сервер() -> Server {
        Server::new(
            crate::registry::Источник::Один {
                имя: "нет".into(),
                путь: PathBuf::from("нет-такого-файла.db"),
            },
            Profile::All,
        )
    }

    #[test]
    fn отвечает_на_initialize() {
        let mut s = сервер();
        let r = s
            .handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .unwrap();
        assert_eq!(r["result"]["serverInfo"]["name"], "gyrfalcon");
        assert_eq!(r["result"]["protocolVersion"], proto::PROTOCOL_VERSION);
    }

    #[test]
    fn отвечает_версией_клиента_когда_она_знакома() {
        // Клиент Streamable HTTP представляется 2025-03-26. Ответить ему
        // версией stdio значило бы разойтись с ним на первом же сообщении.
        let mut s = сервер();
        let r = s
            .handle(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize",
                    "params":{"protocolVersion":"2025-03-26"}}"#,
            )
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
    }

    #[test]
    fn перечисляет_инструменты_по_профилю() {
        let mut s = сервер();
        let r = s
            .handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .unwrap();
        let список = r["result"]["tools"].as_array().unwrap();
        assert_eq!(список.len(), tools::TOOLS.len());
        assert!(список.iter().any(|t| t["name"] == "find"));

        let mut s2 = Server::new(
            crate::registry::Источник::Один {
                имя: "x".into(),
                путь: PathBuf::from("x.db"),
            },
            Profile::Scout,
        );
        let r2 = s2
            .handle(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#)
            .unwrap();
        let мало = r2["result"]["tools"].as_array().unwrap().len();
        assert!(мало < список.len(), "профиль обязан резать набор");
    }

    #[test]
    fn уведомление_остаётся_без_ответа() {
        let mut s = сервер();
        assert!(s
            .handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
    }

    #[test]
    fn неизвестный_метод_даёт_ошибку_протокола() {
        let mut s = сервер();
        let r = s
            .handle(r#"{"jsonrpc":"2.0","id":4,"method":"нет/такого"}"#)
            .unwrap();
        assert_eq!(r["error"]["code"], ErrorCode::MethodNotFound as i32);
    }

    #[test]
    fn битый_json_не_роняет_сервер() {
        let mut s = сервер();
        let r = s.handle("{это не json").unwrap();
        assert_eq!(r["error"]["code"], ErrorCode::ParseError as i32);
    }

    /// Сервер над каталогом с двумя проектами — без единого файла на диске.
    fn каталог(метка: &str, проекты: &[&str]) -> (Server, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("gyrfalcon-srv-{метка}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for п in проекты {
            std::fs::write(d.join(format!("{п}.db")), b"").unwrap();
        }
        (
            Server::new(crate::registry::Источник::Каталог(d.clone()), Profile::All),
            d,
        )
    }

    #[test]
    fn без_project_на_двух_проектах_отказ_с_перечнем() {
        // Худший род ошибки — ответить складно про ЧУЖУЮ конфигурацию.
        // Поэтому молчаливого выбора нет: отказ называет доступные.
        let (mut s, d) = каталог("двое", &["erpuh", "do"]);
        let r = s
            .handle(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
                       "params":{"name":"find","arguments":{"query":"х"}}}"#,
            )
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
        let текст = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(текст.contains("не указан параметр project"), "{текст}");
        assert!(текст.contains("erpuh") && текст.contains("do"), "{текст}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn неизвестный_project_отличим_от_пустого_ответа() {
        let (mut s, d) = каталог("чужой", &["erpuh"]);
        let r = s
            .handle(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call",
                       "params":{"name":"find","arguments":{"query":"х","project":"zup"}}}"#,
            )
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
        let текст = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(текст.contains("не найден"), "{текст}");
        assert!(
            текст.contains("о РЕЕСТРЕ"),
            "про реестр, а не про 1С: {текст}"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn звёздочка_не_ищется_как_имя_проекта() {
        // Дефект, пойманный живой пробой: `project: "*"` — одна строка,
        // то есть «одна цель», и она уходила в поиск проекта с именем `*`.
        // Отказ «проект '*' не найден» при трёх доступных.
        let (mut s, d) = каталог("звезда", &["erpuh", "do"]);
        let r = s
            .handle(
                r#"{"jsonrpc":"2.0","id":9,"method":"tools/call",
                       "params":{"name":"find","arguments":{"query":"х","project":"*"}}}"#,
            )
            .unwrap();
        let текст = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            !текст.contains("'*' не найден"),
            "звёздочка обязана разворачиваться, а не искаться: {текст}"
        );
        // Индексы пустые (файлы-заглушки), поэтому ответ будет с ошибками
        // по каждому проекту — но обёртка by_project обязана быть.
        assert!(текст.contains("by_project"), "{текст}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn list_projects_отвечает_без_указания_проекта() {
        // Инструмент, перечисляющий проекты, сам проекта требовать не может.
        let (mut s, d) = каталог("перечень", &["erpuh", "do"]);
        let r = s
            .handle(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call",
                       "params":{"name":"list_projects","arguments":{}}}"#,
            )
            .unwrap();
        assert_ne!(r["result"]["isError"], true, "отказа быть не должно");
        let текст = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(текст.contains("erpuh"), "{текст}");
        assert!(текст.contains("\"count\":2"), "{текст}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn у_каждого_инструмента_есть_параметр_project() {
        // Проверяется СХЕМА, а не список: параметр добавляется централизованно
        // в `обяз()`, и этот тест ловит инструмент, собравший схему мимо неё.
        let mut s = сервер();
        let r = s
            .handle(r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#)
            .unwrap();
        for t in r["result"]["tools"].as_array().unwrap() {
            let имя = t["name"].as_str().unwrap();
            // Исключения — те, кто к индексу не обращается вовсе:
            // `list_projects` отвечает про реестр серверов, а `check_bsl`
            // разбирает поданный текст. Спрашивать у них проект незачем, и
            // лишний обязательный параметр слабая модель заполнит наугад.
            if имя == "list_projects" || имя == "check_bsl" {
                continue;
            }
            assert!(
                t["inputSchema"]["properties"]["project"].is_object(),
                "у инструмента '{имя}' нет параметра project"
            );
        }
    }

    #[test]
    fn недоступный_индекс_даёт_понятный_отказ_а_не_пустоту() {
        // Ключевое свойство: пустой результат неотличим от «ничего нет»,
        // поэтому недоступный индекс обязан говорить об этом словами.
        let mut s = сервер();
        let r = s
            .handle(
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call",
                       "params":{"name":"find","arguments":{"query":"х"}}}"#,
            )
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
        let текст = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(текст.contains("недоступен"), "получено: {текст}");
        assert!(
            текст.contains("gyrfalcon build"),
            "отказ должен говорить, что делать: {текст}"
        );
    }
}
