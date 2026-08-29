//! Параллельный обход и разбор выгрузки конфигурации.
//!
//! Веха 1 плана: доказать, что параллельный разбор бьёт нынешний сервер.
//! Здесь нет ни базы, ни MCP — только чтение файлов и разбор.
//!
//! # Почему парсер создаётся на каждый поток
//!
//! `tree_sitter::Parser` не потокобезопасен: он несёт изменяемое состояние
//! разбора и не реализует `Sync`. Расшарить один парсер между воркерами нельзя,
//! а создавать его на каждый файл — дорого (загрузка грамматики). Отсюда
//! `thread_local`: один парсер на поток, переиспользуется всеми файлами,
//! которые этому потоку достались.

use crate::bsl::{self, Method};
use rayon::prelude::*;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use walkdir::WalkDir;

thread_local! {
    /// Парсер BSL, свой у каждого потока. `RefCell`, потому что разбор требует
    /// `&mut`, а `thread_local` отдаёт общий доступ.
    static PARSER: RefCell<Option<tree_sitter::Parser>> = const { RefCell::new(None) };
}

/// Выполнить действие с потоковым парсером, создав его при первом обращении.
fn with_parser<T>(f: impl FnOnce(&mut tree_sitter::Parser) -> T) -> Option<T> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut p = tree_sitter::Parser::new();
            p.set_language(&bsl::language()).ok()?;
            *slot = Some(p);
        }
        slot.as_mut().map(f)
    })
}

/// Итог разбора одного модуля.
#[derive(Debug, Clone)]
pub struct ModuleResult {
    pub path: PathBuf,
    pub bytes: u64,
    pub methods: Vec<Method>,
    /// Дерево содержит узлы-ошибки. Не «файл не прочитан» — именно разбор споткнулся.
    pub has_errors: bool,
}

/// Сводка прогона: то, что сравнивается с нынешним сервером.
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub files: u64,
    pub bytes: u64,
    pub methods: u64,
    /// Файлы, которые не удалось прочитать (не путать с ошибками разбора).
    pub unreadable: u64,
    /// Файлы, где разбор дал узлы-ошибки.
    pub with_parse_errors: u64,
    pub elapsed_ms: u64,
    pub threads: usize,
}

impl ScanReport {
    pub fn mb_per_sec(&self) -> f64 {
        if self.elapsed_ms == 0 {
            return 0.0;
        }
        (self.bytes as f64 / 1_048_576.0) / (self.elapsed_ms as f64 / 1000.0)
    }

    pub fn files_per_sec(&self) -> f64 {
        if self.elapsed_ms == 0 {
            return 0.0;
        }
        self.files as f64 / (self.elapsed_ms as f64 / 1000.0)
    }
}

/// Собрать список модулей BSL в каталоге выгрузки.
///
/// Обход отделён от разбора намеренно: он однопоточный (упирается в файловую
/// систему, а не в процессор), и его время меряется отдельно — иначе непонятно,
/// что именно ускорилось.
pub fn collect_modules(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("bsl"))
        })
        .map(walkdir::DirEntry::into_path)
        .collect()
}

/// Разобрать модули параллельно по ядрам.
///
/// Возвращает сводку. Сами результаты не накапливаются: на крупной конфигурации
/// это сотни тысяч методов, и держать их в памяти ради замера скорости незачем.
/// Веха 2 (запись в индекс) будет отдавать их в канал писателю.
pub fn scan_parallel(paths: &[PathBuf]) -> ScanReport {
    let bytes = AtomicU64::new(0);
    let methods = AtomicU64::new(0);
    let unreadable = AtomicU64::new(0);
    let with_errors = AtomicU64::new(0);

    let started = Instant::now();

    paths.par_iter().for_each(|path| {
        let Ok(source) = read_module(path) else {
            unreadable.fetch_add(1, Ordering::Relaxed);
            return;
        };
        bytes.fetch_add(source.len() as u64, Ordering::Relaxed);

        let parsed = with_parser(|parser| {
            let tree = parser.parse(&source, None);
            match tree {
                Some(t) => {
                    let n = count_methods(&t, &source);
                    (n, t.root_node().has_error())
                }
                None => (0, true),
            }
        });

        if let Some((n, had_error)) = parsed {
            methods.fetch_add(n, Ordering::Relaxed);
            if had_error {
                with_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    ScanReport {
        files: paths.len() as u64,
        bytes: bytes.into_inner(),
        methods: methods.into_inner(),
        unreadable: unreadable.into_inner(),
        with_parse_errors: with_errors.into_inner(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        threads: rayon::current_num_threads(),
    }
}

/// Тот же разбор в один поток — база для сравнения.
///
/// Нужен, чтобы ускорение считалось от **нашего же** однопоточного прогона,
/// а не от чужого сервера: иначе в число ускорения попадёт всё подряд,
/// от языка до аллокатора, и станет непонятно, что именно дал параллелизм.
pub fn scan_serial(paths: &[PathBuf]) -> ScanReport {
    let started = Instant::now();
    let mut r = ScanReport {
        files: paths.len() as u64,
        threads: 1,
        ..Default::default()
    };

    for path in paths {
        let Ok(source) = read_module(path) else {
            r.unreadable += 1;
            continue;
        };
        r.bytes += source.len() as u64;

        if let Some((n, had_error)) = with_parser(|parser| match parser.parse(&source, None) {
            Some(t) => (count_methods(&t, &source), t.root_node().has_error()),
            None => (0, true),
        }) {
            r.methods += n;
            if had_error {
                r.with_parse_errors += 1;
            }
        }
    }

    r.elapsed_ms = started.elapsed().as_millis() as u64;
    r
}

/// Прочитать модуль как текст.
///
/// Выгрузки 1С бывают и в UTF-8, и в UTF-8 с BOM. Битые байты заменяются, а не
/// роняют файл: один сбойный символ не повод потерять модуль целиком —
/// но такой файл всё равно попадёт в счётчик ошибок разбора, если сломает разбор.
fn read_module(path: &Path) -> std::io::Result<String> {
    let raw = std::fs::read(path)?;
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
    Ok(String::from_utf8_lossy(raw).into_owned())
}

/// Посчитать объявления процедур и функций в дереве.
fn count_methods(tree: &tree_sitter::Tree, _source: &str) -> u64 {
    let mut n = 0u64;
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if kind == "procedure_definition" || kind == "function_definition" {
            n += 1;
        }
        stack.extend(node.children(&mut cursor));
    }
    n
}

/// Разобрать модули и вернуть результаты целиком — для случаев, когда они нужны.
///
/// На крупной конфигурации это сотни мегабайт: для замера скорости используйте
/// [`scan_parallel`], который считает, но не копит.
pub fn scan_collect(paths: &[PathBuf]) -> Vec<ModuleResult> {
    paths
        .par_iter()
        .filter_map(|path| {
            let source = read_module(path).ok()?;
            let methods = bsl::parse_module(&source).ok()?;
            let has_errors = !bsl::check_syntax(&source).ok()?.is_empty();
            Some(ModuleResult {
                path: path.clone(),
                bytes: source.len() as u64,
                methods,
                has_errors,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn обход_находит_только_bsl() {
        let dir = std::env::temp_dir().join("gyrfalcon-scan-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("вложенный")).unwrap();
        std::fs::write(dir.join("a.bsl"), "Процедура А()\nКонецПроцедуры\n").unwrap();
        std::fs::write(dir.join("вложенный/b.BSL"), "Функция Б()\nКонецФункции\n").unwrap();
        std::fs::write(dir.join("c.xml"), "<x/>").unwrap();

        let found = collect_modules(&dir);
        assert_eq!(found.len(), 2, "нашлось: {found:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn параллельный_и_последовательный_считают_одинаково() {
        let dir = std::env::temp_dir().join("gyrfalcon-scan-eq");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..20 {
            std::fs::write(
                dir.join(format!("m{i}.bsl")),
                "Процедура А() Экспорт\nКонецПроцедуры\nФункция Б()\nКонецФункции\n",
            )
            .unwrap();
        }

        let paths = collect_modules(&dir);
        let par = scan_parallel(&paths);
        let ser = scan_serial(&paths);

        assert_eq!(par.methods, ser.methods, "разное число методов");
        assert_eq!(par.methods, 40, "по 2 метода на 20 файлов");
        assert_eq!(par.files, ser.files);
        assert_eq!(par.bytes, ser.bytes);
        assert_eq!(par.with_parse_errors, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bom_не_ломает_разбор() {
        let dir = std::env::temp_dir().join("gyrfalcon-scan-bom");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("Процедура А()\nКонецПроцедуры\n".as_bytes());
        std::fs::write(dir.join("bom.bsl"), bytes).unwrap();

        let r = scan_parallel(&collect_modules(&dir));
        assert_eq!(r.methods, 1);
        assert_eq!(
            r.with_parse_errors, 0,
            "BOM принят за синтаксическую ошибку"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
