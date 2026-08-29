//! Какой коммит выгрузки лежит в рабочей копии.
//!
//! # Почему чтением файлов, а не вызовом `git`
//!
//! Нужен ровно один факт — SHA текущего HEAD, — и он лежит в `.git/HEAD`
//! открытым текстом. Запуск `git rev-parse` ради него стоил бы порождения
//! процесса (десятки миллисекунд на Windows) на КАЖДЫЙ ответ сервера,
//! требовал бы git в PATH и добавил бы отказ, которого у чтения файла нет.
//!
//! Сервер обязан быть самодостаточным: индекс, который перестаёт отвечать
//! на машине без установленного git, — это отказ там, где его быть не должно.
//!
//! # Что здесь НЕ делается
//!
//! Не читается статус рабочей копии (`git status`) — изменённые файлы ловит
//! обход по mtime в `freshness`, и он честнее: видит правку, не дошедшую до
//! git вовсе. Не разрешается `packed-refs`-ссылка на ветку, которой нет в
//! `refs/heads/`: это редкий случай (свежий клон без чекаута), и на нём
//! функция честно отдаёт `None` вместо выдуманного значения.
//!
//! # Отдельная оговорка про выгрузку 1С
//!
//! Каталог выгрузки часто лежит ВНУТРИ репозитория, а не является его
//! корнем (`repo/src/cf/...`). Поэтому `.git` ищется вверх по дереву, а не
//! только в переданном каталоге.

use std::path::Path;

/// Максимум уровней вверх при поиске `.git`.
///
/// Ограничение, а не обход до корня диска: без него функция на каталоге вне
/// репозитория прошла бы всю цепочку родителей до `C:\` на каждом вызове.
const ВВЕРХ: usize = 12;

/// SHA текущего HEAD для каталога (или любого его предка с `.git`).
///
/// `None` — не репозиторий, или HEAD прочитать не удалось. Это НЕ ошибка:
/// выгрузка вполне может лежать вне git, и тогда признак отставания у нас
/// один — mtime.
pub fn head(dir: &Path) -> Option<String> {
    let git = найти_git(dir)?;
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();

    // Detached HEAD: в файле сразу SHA.
    if let Some(sha) = проверить_sha(head) {
        return Some(sha);
    }

    // Обычный случай: `ref: refs/heads/<ветка>`.
    let ссылка = head.strip_prefix("ref:")?.trim();
    if let Ok(s) = std::fs::read_to_string(git.join(ссылка)) {
        if let Some(sha) = проверить_sha(s.trim()) {
            return Some(sha);
        }
    }
    // Ветка упакована: `.git/packed-refs`, строки «<sha> <ref>».
    let packed = std::fs::read_to_string(git.join("packed-refs")).ok()?;
    for строка in packed.lines() {
        let строка = строка.trim();
        if строка.starts_with('#') || строка.starts_with('^') {
            continue;
        }
        let (sha, имя) = строка.split_once(' ')?;
        if имя.trim() == ссылка {
            return проверить_sha(sha);
        }
    }
    None
}

/// Каталог `.git` для пути — свой или ближайшего предка.
///
/// Понимает и файл `.git` вместо каталога: так выглядит рабочее дерево
/// (`worktree`) и подмодуль, где внутри лежит `gitdir: <путь>`.
fn найти_git(dir: &Path) -> Option<std::path::PathBuf> {
    let mut текущий = dir;
    for _ in 0..ВВЕРХ {
        let кандидат = текущий.join(".git");
        if кандидат.is_dir() {
            return Some(кандидат);
        }
        if кандидат.is_file() {
            let s = std::fs::read_to_string(&кандидат).ok()?;
            let путь = s.trim().strip_prefix("gitdir:")?.trim();
            let p = std::path::PathBuf::from(путь);
            return Some(if p.is_absolute() {
                p
            } else {
                текущий.join(p)
            });
        }
        текущий = текущий.parent()?;
    }
    None
}

/// Строка похожа на SHA-1/SHA-256 коммита.
///
/// Проверка нужна, потому что прочитанное могло оказаться чем угодно —
/// пустым файлом, `ref:` на несуществующее, обрывком. Записать в индекс
/// мусор под видом коммита значит потом сравнивать его с настоящим и
/// вечно показывать отставание.
fn проверить_sha(s: &str) -> Option<String> {
    let s = s.trim();
    let годится = (s.len() == 40 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit());
    годится.then(|| s.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// Каталог теста, убирающий за собой.
    struct Врем(PathBuf);
    impl Врем {
        fn новый(метка: &str) -> Self {
            let d = std::env::temp_dir().join(format!("gyrfalcon-git-{метка}"));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Врем {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn репозиторий(метка: &str) -> Врем {
        let d = Врем::новый(метка);
        fs::create_dir_all(d.path().join(".git/refs/heads")).unwrap();
        d
    }

    #[test]
    fn читает_ветку_через_ref() {
        let d = репозиторий("ref");
        fs::write(d.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(d.path().join(".git/refs/heads/main"), format!("{SHA}\n")).unwrap();
        assert_eq!(head(d.path()).as_deref(), Some(SHA));
    }

    #[test]
    fn читает_detached_head() {
        let d = репозиторий("detached");
        fs::write(d.path().join(".git/HEAD"), format!("{SHA}\n")).unwrap();
        assert_eq!(head(d.path()).as_deref(), Some(SHA));
    }

    #[test]
    fn читает_упакованную_ссылку() {
        let d = репозиторий("packed");
        fs::write(d.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            d.path().join(".git/packed-refs"),
            format!("# pack-refs with: peeled\n{SHA} refs/heads/main\n"),
        )
        .unwrap();
        assert_eq!(head(d.path()).as_deref(), Some(SHA));
    }

    #[test]
    fn находит_git_у_предка() {
        // Выгрузка 1С лежит внутри репозитория, а не в его корне.
        let d = репозиторий("предок");
        fs::write(d.path().join(".git/HEAD"), format!("{SHA}\n")).unwrap();
        let выгрузка = d.path().join("src/cf");
        fs::create_dir_all(&выгрузка).unwrap();
        assert_eq!(head(&выгрузка).as_deref(), Some(SHA));
    }

    #[test]
    fn не_репозиторий_даёт_none_а_не_выдумку() {
        let d = Врем::новый("пусто");
        assert_eq!(head(d.path()), None);
    }

    #[test]
    fn мусор_вместо_sha_не_принимается() {
        let d = репозиторий("мусор");
        fs::write(d.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(d.path().join(".git/refs/heads/main"), "не-коммит\n").unwrap();
        assert_eq!(head(d.path()), None, "мусор под видом коммита хуже none");
    }

    #[test]
    fn понимает_gitdir_файл_рабочего_дерева() {
        let d = Врем::новый("worktree");
        let реальный = d.path().join("настоящий-git");
        fs::create_dir_all(&реальный).unwrap();
        fs::write(реальный.join("HEAD"), format!("{SHA}\n")).unwrap();
        let дерево = d.path().join("дерево");
        fs::create_dir_all(&дерево).unwrap();
        fs::write(
            дерево.join(".git"),
            format!("gitdir: {}\n", реальный.display()),
        )
        .unwrap();
        assert_eq!(head(&дерево).as_deref(), Some(SHA));
    }
}
