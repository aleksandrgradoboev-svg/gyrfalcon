//! Хуки харнеса: подсказка вместо запрета.
//!
//! # Устройство взято у образца и оно намеренно НЕ запрещающее
//!
//! Хук на `Grep`/`Glob` **никогда не блокирует вызов**. Он добавляет в контекст
//! строку «есть индекс, спроси его» — и всё. Любая неудача молчаливая:
//! бинаря нет, версия старая, индекс недоступен → `exit 0` без вывода.
//!
//! Причина такая: хук, который умеет сломать чужую работу, однажды её сломает,
//! и разбираться будут не с индексом, а с тем, почему перестал работать поиск.
//! Подсказка, которую можно проигнорировать, зарабатывает доверие; запрет,
//! который нельзя обойти, зарабатывает обходные пути.
//!
//! **Отличие от стража 1С в контуре — не в цене поиска.** Он разорителен
//! в обоих случаях. Отличие в том, что там альтернатива гарантирована
//! (индекс покрывает платформу целиком) и компьютер свой, а здесь продукт
//! ставится посторонним людям на их проекты, где запрещать им их же `grep`
//! было бы наглостью. Подробнее — в шапке `install`.
//!
//! # Тонкая обёртка, логика в бинаре
//!
//! Скрипт хука — три строки: проверить бинарь, вызвать `gyrfalcon hook-augment`,
//! выйти нулём. Логика живёт в Rust, а не в shell: скрипт, размазанный по
//! конфигу харнеса, не тестируется и расходится с кодом молча.

use std::path::{Path, PathBuf};

/// Событие харнеса, к которому цепляется хук.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Перед `Grep`/`Glob` — подсказать, что есть индекс.
    BeforeSearch,
    /// Старт сессии — напомнить про индекс и его свежесть.
    SessionStart,
    /// Старт субагента — то же самое для дочерней роли.
    SubagentStart,
}

impl Event {
    pub const ALL: &'static [Event] = &[
        Event::BeforeSearch,
        Event::SessionStart,
        Event::SubagentStart,
    ];

    /// Имя файла-обёртки.
    pub fn script_name(self) -> &'static str {
        match self {
            Event::BeforeSearch => "gyrfalcon-search-hint",
            Event::SessionStart => "gyrfalcon-session-hint",
            Event::SubagentStart => "gyrfalcon-subagent-hint",
        }
    }

    /// Подкоманда бинаря, которую зовёт обёртка.
    pub fn subcommand(self) -> &'static str {
        match self {
            Event::BeforeSearch => "hook-augment",
            Event::SessionStart => "hook-session",
            Event::SubagentStart => "hook-subagent",
        }
    }

    /// Значение `matcher` в конфиге Claude Code.
    ///
    /// Нужно тому, кто вписывает хук в конфиг руками: установщик печатает
    /// фрагмент, а не правит чужой файл.
    pub fn matcher(self) -> Option<&'static str> {
        match self {
            Event::BeforeSearch => Some("Grep|Glob"),
            _ => None,
        }
    }

    pub fn hook_event(self) -> &'static str {
        match self {
            Event::BeforeSearch => "PreToolUse",
            Event::SessionStart => "SessionStart",
            Event::SubagentStart => "SubagentStart",
        }
    }
}

/// Текст обёртки под POSIX-оболочку.
pub fn script_sh(binary: &str, e: Event) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         # gyrfalcon: подсказка про индекс 1С. НИКОГДА не блокирует вызов —\n\
         # любая неудача молчаливая (exit 0, без вывода).\n\
         BIN=\"{binary}\"\n\
         [ -x \"$BIN\" ] || exit 0\n\
         \"$BIN\" {} 2>/dev/null\n\
         exit 0\n",
        e.subcommand()
    )
}

/// Текст обёртки под Windows.
pub fn script_cmd(binary: &str, e: Event) -> String {
    format!(
        "@echo off\r\n\
         REM gyrfalcon: подсказка про индекс 1С. НИКОГДА не блокирует вызов.\r\n\
         if not exist \"{binary}\" exit /b 0\r\n\
         \"{binary}\" {} 2>nul\r\n\
         exit /b 0\r\n",
        e.subcommand()
    )
}

/// Путь обёртки в каталоге хуков харнеса.
pub fn script_path(hooks_dir: &Path, e: Event) -> PathBuf {
    let имя = if cfg!(windows) {
        format!("{}.cmd", e.script_name())
    } else {
        e.script_name().to_string()
    };
    hooks_dir.join(имя)
}

/// Ответ хука перед поиском.
///
/// Пустая строка = молчание. Возвращается, когда индекса нет: подсказывать
/// про инструмент, которого не поднять, — шум, а не помощь.
pub fn augment_text(индекс_есть: bool) -> String {
    if !индекс_есть {
        return String::new();
    }
    "Для этой конфигурации 1С собран индекс gyrfalcon. Поиск по выгрузке \
     (Grep/Glob) даёт мусор и стоит десятки тысяч токенов: имена в XML \
     разбросаны, связи модели в тексте не видны. Спросите индекс: find — найти \
     объект, метод или модуль (в том числе по смыслу); object — реквизиты \
     с типами и движения; callers — граф вызовов; overrides — перехваты \
     расширений. Прежде чем заключить «такого нет» — coverage."
        .to_string()
}

/// Ответ хука на старте сессии или субагента.
pub fn session_text(индекс: Option<&str>) -> String {
    match индекс {
        None => String::new(),
        Some(путь) => format!(
            "gyrfalcon: индекс {путь}. Навигация по конфигурации 1С — через него, \
             а не чтением выгрузки. Правило вызова — в скилле gyrfalcon."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn обёртка_выходит_нулём_при_любой_беде() {
        let s = script_sh("/usr/local/bin/gyrfalcon", Event::BeforeSearch);
        assert!(s.contains("exit 0"), "хук обязан завершаться нулём");
        assert!(
            s.contains("[ -x \"$BIN\" ] || exit 0"),
            "пропавший бинарь не должен ломать вызов"
        );
        assert!(s.contains("hook-augment"));
    }

    #[test]
    fn windows_обёртка_тоже_не_блокирует() {
        let s = script_cmd("C:/bin/gyrfalcon.exe", Event::SessionStart);
        assert!(s.contains("exit /b 0"));
        assert!(s.contains("hook-session"));
    }

    #[test]
    fn без_индекса_хук_молчит() {
        // Подсказка про недоступный инструмент — шум, который приучает
        // не читать подсказки.
        assert_eq!(augment_text(false), "");
        assert_eq!(session_text(None), "");
    }

    #[test]
    fn подсказка_называет_инструменты_и_оговорку_о_полноте() {
        let t = augment_text(true);
        for i in ["find", "object", "callers", "overrides", "coverage"] {
            assert!(t.contains(i), "подсказка не называет {i}");
        }
    }

    #[test]
    fn у_события_поиска_есть_matcher_у_остальных_нет() {
        assert_eq!(Event::BeforeSearch.matcher(), Some("Grep|Glob"));
        assert!(Event::SessionStart.matcher().is_none());
    }

    #[test]
    fn имена_подкоманд_уникальны() {
        let mut n: Vec<&str> = Event::ALL.iter().map(|e| e.subcommand()).collect();
        n.sort_unstable();
        let было = n.len();
        n.dedup();
        assert_eq!(было, n.len());
    }
}
