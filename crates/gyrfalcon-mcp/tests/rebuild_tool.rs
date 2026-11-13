//! `rebuild_index` (MCP-04, этап А) — на настоящем бинаре `gyrfalcon`.
//!
//! Интеграционный тест, а не модульный: пересборка запускает процесс того же
//! бинаря, и путь к нему cargo даёт только интеграционным тестам.

use gyrfalcon_mcp::registry::Источник;
use gyrfalcon_mcp::server::Server;
use gyrfalcon_mcp::tools::Profile;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn каталог(метка: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "gyrfalcon-rebuild-tool-{метка}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Выгрузка из одного общего модуля с одним методом.
fn выгрузка(корень: &Path, метод: &str) {
    let м = корень.join("src/CommonModules/Мод/Ext");
    std::fs::create_dir_all(&м).unwrap();
    std::fs::write(
        корень.join("src/Configuration.xml"),
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<MetaDataObject/>",
    )
    .unwrap();
    std::fs::write(
        м.join("Module.bsl"),
        format!("Процедура {метод}() Экспорт\nКонецПроцедуры\n"),
    )
    .unwrap();
}

fn сервер(индекс: &Path, бинарь: Option<PathBuf>) -> Server {
    Server::new(
        Источник::Один {
            имя: "t".into(),
            путь: индекс.to_path_buf(),
        },
        Profile::All,
    )
    .с_пересборкой(бинарь)
}

/// Вызов инструмента: (ошибка ли, текст ответа).
fn вызов(s: &Server, имя: &str, args: Value) -> (bool, String) {
    let запрос = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": имя, "arguments": args}
    });
    let r = s.handle(&запрос.to_string()).unwrap();
    (
        r["result"]["isError"] == true,
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

fn инструменты(s: &Server) -> Vec<String> {
    let r = s
        .handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .unwrap();
    r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

fn стенд(метка: &str) -> (PathBuf, PathBuf) {
    let d = каталог(метка);
    выгрузка(&d, "Первая");
    let индекс = d.join("idx/t.db");
    gyrfalcon_index::build(&d.join("src"), &индекс, None).unwrap();
    (d, индекс)
}

#[test]
fn без_флага_инструмента_нет() {
    let (d, индекс) = стенд("off");
    let s = сервер(&индекс, None);
    assert!(!инструменты(&s).iter().any(|n| n == "rebuild_index"));
    let (ошибка, т) = вызов(&s, "rebuild_index", serde_json::json!({"project": "t"}));
    assert!(ошибка && т.contains("--allow-rebuild"), "{т}");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn пересборка_по_вызову_и_итог_в_list_projects() {
    let (d, индекс) = стенд("run");
    let s = сервер(
        &индекс,
        Some(PathBuf::from(env!("CARGO_BIN_EXE_gyrfalcon"))),
    );
    assert!(инструменты(&s).iter().any(|n| n == "rebuild_index"));
    let (_, до) = вызов(
        &s,
        "sql",
        serde_json::json!({"query": "SELECT name FROM methods"}),
    );
    assert!(до.contains("Первая"), "{до}");

    выгрузка(&d, "Вторая");
    let (ошибка, т) = вызов(&s, "rebuild_index", serde_json::json!({"project": "t"}));
    assert!(!ошибка, "{т}");
    assert!(т.contains("\"started\""), "{т}");

    // Итог приходит в list_projects, когда процесс закончился.
    let до_срока = Instant::now() + Duration::from_secs(120);
    let итог = loop {
        let (_, список) = вызов(&s, "list_projects", serde_json::json!({}));
        let v: Value = serde_json::from_str(&список).unwrap();
        let свежесть = &v["rows"][0][5];
        if !свежесть["last_rebuild"].is_null() {
            break свежесть["last_rebuild"].clone();
        }
        assert!(Instant::now() < до_срока, "итога нет: {список}");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(итог["status"], "completed", "{итог}");

    let (ошибка, после) = вызов(
        &s,
        "sql",
        serde_json::json!({"query": "SELECT name FROM methods"}),
    );
    assert!(!ошибка, "{после}");
    assert!(
        после.contains("Вторая") && !после.contains("Первая"),
        "{после}"
    );
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn звёздочка_и_отмена_несуществующей_отказывают() {
    let (d, индекс) = стенд("refuse");
    let s = сервер(
        &индекс,
        Some(PathBuf::from(env!("CARGO_BIN_EXE_gyrfalcon"))),
    );
    let (ошибка, т) = вызов(&s, "rebuild_index", serde_json::json!({"project": "*"}));
    assert!(ошибка && т.contains("по одному проекту"), "{т}");
    let (ошибка, т) = вызов(
        &s,
        "rebuild_index",
        serde_json::json!({"project": "t", "cancel": true}),
    );
    assert!(ошибка && т.contains("не идёт"), "{т}");
    // Булево строкой — та же отмена, а не запуск сборки.
    let (ошибка, т) = вызов(
        &s,
        "rebuild_index",
        serde_json::json!({"project": "t", "cancel": "true"}),
    );
    assert!(ошибка && т.contains("не идёт"), "{т}");
    let (ошибка, т) = вызов(
        &s,
        "rebuild_index",
        serde_json::json!({"project": "t", "cancel": "yes"}),
    );
    assert!(ошибка && т.contains("ожидается true или false"), "{т}");
    assert!(
        !gyrfalcon_index::rebuild::путь_метки(&индекс).exists(),
        "ни одна из этих просьб сборку не запустила"
    );
    let _ = std::fs::remove_dir_all(d);
}

/// Долгая «сборка» — вместо бинаря скрипт, который просто ждёт.
fn долгая_сборка(d: &Path) -> PathBuf {
    if cfg!(windows) {
        let p = d.join("slow.cmd");
        std::fs::write(&p, "@ping -n 31 127.0.0.1 >nul\r\n").unwrap();
        p
    } else {
        let p = d.join("slow.sh");
        std::fs::write(&p, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

#[test]
fn отмена_останавливает_сборку_и_не_трогает_чужую_метку() {
    let (d, индекс) = стенд("cancel");
    let s = сервер(&индекс, Some(долгая_сборка(&d)));
    let (ошибка, т) = вызов(&s, "rebuild_index", serde_json::json!({"project": "t"}));
    assert!(!ошибка, "{т}");
    // Повторный запуск, пока идёт, — отказ.
    let (ошибка, т) = вызов(&s, "rebuild_index", serde_json::json!({"project": "t"}));
    assert!(ошибка && т.contains("уже идёт"), "{т}");

    // Метка чужого процесса (другой pid) отменой не убирается.
    let метка = gyrfalcon_index::rebuild::путь_метки(&индекс);
    let сейчас = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        &метка,
        format!(r#"{{"pid":1,"started":{сейчас},"beat":{сейчас},"expected_ms":null}}"#),
    )
    .unwrap();

    let начало = Instant::now();
    let (ошибка, т) = вызов(
        &s,
        "rebuild_index",
        serde_json::json!({"project": "t", "cancel": true}),
    );
    assert!(!ошибка, "{т}");
    assert!(т.contains("cancelled"), "{т}");
    assert!(
        начало.elapsed() < Duration::from_secs(10),
        "отмена не ждёт конца сборки"
    );
    assert!(метка.exists(), "чужая метка не тронута");
    std::fs::remove_file(&метка).unwrap();

    let (_, список) = вызов(&s, "list_projects", serde_json::json!({}));
    assert!(список.contains("\"cancelled\""), "{список}");
    let _ = std::fs::remove_dir_all(d);
}
