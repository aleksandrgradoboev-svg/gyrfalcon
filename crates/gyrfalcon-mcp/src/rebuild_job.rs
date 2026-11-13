//! Пересборка индекса по вызову MCP (MCP-04, этап А).
//!
//! # Что это добавляет к пересборке на месте (MCP-07 п. 11)
//!
//! Механику — метку, отказ на время сборки, подмену файла — дал п. 11.
//! Здесь только **запуск**: агент просит пересобрать проект по имени и сразу
//! получает ответ «запущено», не зная ни пути к исходникам, ни пути к индексу,
//! ни словаря, и не имея оболочки вовсе. Ход видно по метке (`rebuilding` в
//! `list_projects`), итог последней пересборки — `last_rebuild` там же.
//!
//! # Почему отдельный процесс, а не поток сервера
//!
//! Сборка занимает все ядра на минуты. В потоке сервера она отнимала бы их у
//! вызовов по остальным проектам, а остановить поток посреди записи SQLite
//! нечем. Процесс — тот же бинарь с командой `build`: отмена — это остановка
//! процесса, а п. 11 гарантирует, что живой индекс при этом цел.
//!
//! # Почему не MCP Tasks
//!
//! Расширение `io.modelcontextprotocol/tasks` живёт поверх ревизии 2026-07-28,
//! а клиенту, который его не заявил, сервер задачу вернуть не вправе. Из
//! клиентов контура его не умеет никто, кроме Claude Code в терминале (13.11.2026).
//! Мгновенное «запущено» — обычный ответ инструмента, его понимает любой клиент.
//! Задачи по стандарту, когда понадобятся, завернут этот же механизм.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gyrfalcon_index::rebuild;

/// Сколько последних строк журнала сборки показывать в итоге.
const ХВОСТ_СТРОК: usize = 15;

/// Пересборки, запущенные этим сервером.
pub struct Сборки {
    /// Бинарь, которым запускается `build`. Сервер передаёт свой
    /// (`current_exe`), тесты — собранный cargo.
    бинарь: PathBuf,
    задания: Mutex<HashMap<PathBuf, Задание>>,
}

struct Задание {
    процесс: Option<Child>,
    pid: u32,
    начато: Instant,
    начато_эпоха: u64,
    журнал: PathBuf,
    итог: Option<Value>,
}

fn сейчас() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Журнал сборки рядом с индексом. Не `.db` — реестр проектов его не возьмёт.
pub fn путь_журнала(индекс: &Path) -> PathBuf {
    let mut s = индекс.as_os_str().to_os_string();
    s.push(".build.log");
    PathBuf::from(s)
}

/// Есть ли в индексе словарь семантики — тогда он и есть словарь для
/// пересборки. Словарь общий для всех конфигураций и при сборке копируется
/// ВНУТРЬ индекса; без него семантика ушла бы целиком в random indexing,
/// то есть пересборка тихо ухудшила бы смысловой поиск.
fn есть_словарь(индекс: &Path) -> bool {
    rusqlite::Connection::open_with_flags(индекс, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|c| {
            c.query_row("SELECT count(*) FROM semantic_dictionary", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .map(|n| n > 0)
        .unwrap_or(false)
}

fn хвост(журнал: &Path) -> String {
    let текст = std::fs::read(журнал).unwrap_or_default();
    let текст = String::from_utf8_lossy(&текст);
    let строки: Vec<&str> = текст.lines().filter(|s| !s.trim().is_empty()).collect();
    строки[строки.len().saturating_sub(ХВОСТ_СТРОК)..].join("\n")
}

impl Задание {
    /// Снять итог, если процесс закончился. Идёт — `false`.
    fn обновить(&mut self) -> bool {
        let Some(п) = self.процесс.as_mut() else {
            return true;
        };
        match п.try_wait() {
            Ok(None) => false,
            Ok(Some(код)) => {
                let успех = код.success();
                self.итог = Some(json!({
                    "status": if успех { "completed" } else { "failed" },
                    "exit_code": код.code(),
                    "started": self.начато_эпоха,
                    "seconds": self.начато.elapsed().as_secs(),
                    "log": self.журнал.display().to_string(),
                    "log_tail": хвост(&self.журнал),
                }));
                self.процесс = None;
                true
            }
            Err(e) => {
                self.итог = Some(json!({
                    "status": "failed",
                    "error": format!("состояние процесса сборки не снять: {e}"),
                    "log": self.журнал.display().to_string(),
                }));
                self.процесс = None;
                true
            }
        }
    }
}

impl Сборки {
    pub fn new(бинарь: PathBuf) -> Self {
        Self {
            бинарь,
            задания: Mutex::new(HashMap::new()),
        }
    }

    fn задания(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Задание>> {
        self.задания
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Запустить полную пересборку индекса.
    pub fn запустить(&self, индекс: &Path) -> Result<Value, String> {
        let mut задания = self.задания();
        if let Some(з) = задания.get_mut(индекс) {
            if !з.обновить() {
                return Err(format!(
                    "пересборка {} уже идёт (pid {}, {} с) — ход в list_projects, поле rebuilding",
                    индекс.display(),
                    з.pid,
                    з.начато.elapsed().as_secs()
                ));
            }
        }
        // Сборку мог запустить и не этот сервер — из оболочки, другим сервером.
        if let Some(м) = rebuild::живая(индекс) {
            return Err(rebuild::отказ(индекс, &м));
        }
        let сведения = gyrfalcon_index::build::info(индекс)
            .map_err(|e| format!("индекс {} не читается: {e}", индекс.display()))?;
        let исходники = PathBuf::from(&сведения.source_path);
        if сведения.source_path.is_empty() || !исходники.is_dir() {
            return Err(format!(
                "каталога исходников, по которому собран индекс, нет: '{}'. \
                 Пересобрать можно только там, где лежит выгрузка",
                сведения.source_path
            ));
        }

        let журнал = путь_журнала(индекс);
        let файл = std::fs::File::create(&журнал)
            .map_err(|e| format!("журнал сборки {} не создать: {e}", журнал.display()))?;
        let ошибки = файл
            .try_clone()
            .map_err(|e| format!("журнал сборки: {e}"))?;

        let mut команда = Command::new(&self.бинарь);
        команда
            .arg("build")
            .arg(&исходники)
            .arg("--out")
            .arg(индекс);
        let словарь = есть_словарь(индекс);
        if словарь {
            команда.arg("--dict").arg(индекс);
        }
        команда
            .stdin(Stdio::null())
            .stdout(Stdio::from(файл))
            .stderr(Stdio::from(ошибки));
        #[cfg(windows)]
        {
            // Без окна консоли: сервер часто работает скрытым, и окно сборки
            // всплыло бы перед человеком.
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            команда.creation_flags(CREATE_NO_WINDOW);
        }
        let процесс = команда
            .spawn()
            .map_err(|e| format!("сборку не запустить ({}): {e}", self.бинарь.display()))?;
        let pid = процесс.id();
        let ожидание = rebuild::прошлая_длительность(индекс).map(|ms| ms.div_ceil(1000));
        задания.insert(
            индекс.to_path_buf(),
            Задание {
                процесс: Some(процесс),
                pid,
                начато: Instant::now(),
                начато_эпоха: сейчас(),
                журнал: журнал.clone(),
                итог: None,
            },
        );
        Ok(json!({
            "status": "started",
            "pid": pid,
            "source_path": исходники.display().to_string(),
            "dictionary": if словарь { "из индекса" } else { "нет — семантика без словаря" },
            "expected_seconds": ожидание,
            "log": журнал.display().to_string(),
            "note": "Сборка идёт отдельным процессом; вызовы по этому проекту до её конца \
                     получают отказ «пересобирается». Ход — list_projects, поле rebuilding; \
                     итог — поле last_rebuild там же. Остановить — rebuild_index с cancel: true"
        }))
    }

    /// Остановить пересборку, запущенную этим сервером.
    ///
    /// Процесс останавливается снаружи, поэтому за собой он не уберёт: метку
    /// (только СВОЮ — по pid) и недособранный `.new` убирает сервер. Живой
    /// индекс сборка до подмены не трогает (п. 11).
    pub fn отменить(&self, индекс: &Path) -> Result<Value, String> {
        let mut задания = self.задания();
        let идёт = match задания.get_mut(индекс) {
            Some(з) => !з.обновить(),
            None => false,
        };
        if !идёт {
            return match rebuild::живая(индекс) {
                Some(м) => Err(format!(
                    "пересборку {} запустил не этот сервер (pid {}) — остановить её можно \
                     только там, где её запустили",
                    индекс.display(),
                    м.pid
                )),
                None => Err(format!(
                    "пересборка {} не идёт — останавливать нечего",
                    индекс.display()
                )),
            };
        }
        let з = задания.get_mut(индекс).expect("проверено выше");
        let mut процесс = з.процесс.take().expect("идёт — значит процесс есть");
        let _ = процесс.kill();
        let код = процесс.wait().ok();
        // Сборка могла закончиться сама между проверкой и остановкой — и уже
        // подменить индекс. Успешный код выхода у убитого процесса не бывает
        // (Windows: код 1, Unix: сигнал), поэтому успех значит «доделала сама»,
        // и итог обязан это сказать, а не «отменено, индекс не тронут».
        if код.is_some_and(|к| к.success()) {
            let итог = json!({
                "status": "completed",
                "exit_code": 0,
                "started": з.начато_эпоха,
                "seconds": з.начато.elapsed().as_secs(),
                "log": з.журнал.display().to_string(),
                "log_tail": хвост(&з.журнал),
            });
            з.итог = Some(итог.clone());
            return Err(format!(
                "пересборка {} успела закончиться до отмены — индекс уже новый; \
                 итог: {итог}",
                индекс.display()
            ));
        }
        // Метка остановленной сборки больше не пульсирует, но мёртвой станет
        // только через `ЖИВА_СЕКУНД` — всё это время проект отказывал бы.
        // Убирается только своя: чужую сборку (другой pid) не трогаем.
        let наша = std::fs::read_to_string(rebuild::путь_метки(индекс))
            .ok()
            .and_then(|t| serde_json::from_str::<rebuild::СостояниеМетки>(&t).ok())
            .is_some_and(|м| м.pid == з.pid);
        if наша {
            let _ = std::fs::remove_file(rebuild::путь_метки(индекс));
        }
        let _ = std::fs::remove_file(rebuild::путь_сборки(индекс));
        let итог = json!({
            "status": "cancelled",
            "started": з.начато_эпоха,
            "seconds": з.начато.elapsed().as_secs(),
            "log": з.журнал.display().to_string(),
            "log_tail": хвост(&з.журнал),
        });
        з.итог = Some(итог.clone());
        Ok(json!({
            "status": "cancelled",
            "note": "сборка остановлена, живой индекс не тронут",
            "last_rebuild": итог
        }))
    }

    /// Итог последней пересборки этого индекса этим сервером, если она была
    /// и уже закончилась.
    pub fn итог(&self, индекс: &Path) -> Option<Value> {
        let mut задания = self.задания();
        let з = задания.get_mut(индекс)?;
        if з.обновить() {
            з.итог.clone()
        } else {
            None
        }
    }
}
