//! Крошечный HTTP-сервер под визуальную часть.
//!
//! # Почему свой, а не крейт
//!
//! Нужно отдать одну страницу и четыре JSON-ответа локальному браузеру.
//! Веб-фреймворк ради этого притащил бы асинхронную среду и десятки
//! зависимостей в проект, где весь остальной код синхронный.
//!
//! # Границы, названные прямо
//!
//! Сервер **слушает только localhost** и предназначен для одного человека
//! за своим компьютером. Это не веб-приложение: нет аутентификации, нет
//! ограничения запросов, нет TLS. Выставлять его наружу нельзя — и он
//! физически не даст этого сделать, потому что привязывается к `127.0.0.1`.

use crate::ui;
use rusqlite::Connection;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

/// Запустить сервер и слушать, пока не прервут.
pub fn serve(db: std::path::PathBuf, port: u16) -> Result<(), String> {
    // Соединение открывается сразу: если индекса нет, лучше сказать об этом
    // до того, как человек откроет браузер и увидит пустую страницу.
    let conn = crate::sql::open_readonly(&db).map_err(|e| {
        format!(
            "индекс {} недоступен: {e}. Соберите его: gyrfalcon build <путь> --out <файл>",
            db.display()
        )
    })?;

    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr)
        .map_err(|e| format!("не занять порт {port}: {e}. Другой — параметром --port"))?;

    println!("gyrfalcon UI: http://{addr}");
    println!("индекс: {}", db.display());
    println!("Ctrl+C — остановить.");

    for поток in listener.incoming() {
        match поток {
            Ok(s) => {
                if let Err(e) = обслужить(s, &conn) {
                    eprintln!("запрос не обслужен: {e}");
                }
            }
            Err(e) => eprintln!("соединение не принято: {e}"),
        }
    }
    Ok(())
}

fn обслужить(mut s: TcpStream, conn: &Connection) -> std::io::Result<()> {
    let mut r = BufReader::new(s.try_clone()?);
    let mut строка = String::new();
    r.read_line(&mut строка)?;
    let путь = строка.split_whitespace().nth(1).unwrap_or("/").to_string();

    // Заголовки дочитываем, чтобы не оставить их в сокете: иначе браузер
    // получит ответ на середине собственного запроса.
    let mut h = String::new();
    while r.read_line(&mut h)? > 2 {
        h.clear();
    }

    let (тип, тело) = маршрут(&путь, conn);
    let ответ = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {тип}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        тело.len()
    );
    s.write_all(ответ.as_bytes())?;
    s.write_all(&тело)?;
    s.flush()
}

fn маршрут(путь: &str, conn: &Connection) -> (&'static str, Vec<u8>) {
    let (адрес, запрос) = match путь.split_once('?') {
        Some((a, q)) => (a, q),
        None => (путь, ""),
    };
    let limit = запрос
        .split('&')
        .find_map(|p| p.strip_prefix("limit="))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(60)
        .clamp(1, 500);

    let json = |r: Result<serde_json::Value, String>| -> (&'static str, Vec<u8>) {
        let v = match r {
            Ok(v) => v,
            // Ошибка приходит в браузер текстом, а не пустотой: пустой ответ
            // страница нарисует как «данных нет», и это будет неправдой.
            Err(e) => serde_json::json!({"error": e}),
        };
        (
            "application/json; charset=utf-8",
            serde_json::to_vec(&v).unwrap_or_default(),
        )
    };

    match адрес {
        "/" | "/index.html" => ("text/html; charset=utf-8", ui::PAGE.as_bytes().to_vec()),
        "/api/summary" => json(ui::summary_data(conn)),
        "/api/movements" => json(ui::movements_data(conn, limit)),
        "/api/subsystems" => json(ui::subsystems_data(conn, limit)),
        "/api/overrides" => json(ui::overrides_data(conn)),
        _ => (
            "text/plain; charset=utf-8",
            format!("нет такого адреса: {адрес}").into_bytes(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn база() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE modules (id INTEGER); CREATE TABLE methods (id INTEGER);
             CREATE TABLE calls (callee_key TEXT); CREATE TABLE object_attributes (object_name TEXT);
             CREATE TABLE register_movements (document_name TEXT, register_name TEXT, source TEXT);
             CREATE TABLE subsystem_content (subsystem_name TEXT, subsystem_synonym TEXT,
               object_ref TEXT, file TEXT);
             CREATE TABLE extension_overrides (extension_name TEXT, extension_purpose TEXT,
               object_name TEXT, target_method TEXT, annotation TEXT);
             CREATE TABLE index_meta (key TEXT, value TEXT);",
        )
        .unwrap();
        c
    }

    #[test]
    fn корень_отдаёт_страницу() {
        let (тип, тело) = маршрут("/", &база());
        assert!(тип.starts_with("text/html"));
        assert!(тело.len() > 2000);
    }

    #[test]
    fn апи_отдаёт_json() {
        for a in [
            "/api/summary",
            "/api/movements",
            "/api/subsystems",
            "/api/overrides",
        ] {
            let (тип, тело) = маршрут(a, &база());
            assert!(тип.starts_with("application/json"), "{a}");
            let v: serde_json::Value = serde_json::from_slice(&тело).expect(a);
            assert!(v.is_object(), "{a}");
        }
    }

    #[test]
    fn лимит_читается_из_запроса_и_ограничен() {
        // 10000 просить можно, получить — нет: страница не для того, чтобы
        // выгрузить в браузер весь индекс.
        let (_, тело) = маршрут("/api/movements?limit=10000", &база());
        let v: serde_json::Value = serde_json::from_slice(&тело).unwrap();
        assert_eq!(v["shown_limit"], 500);
    }

    #[test]
    fn неизвестный_адрес_говорит_об_этом() {
        let (тип, тело) = маршрут("/чего-то-нет", &база());
        assert!(тип.starts_with("text/plain"));
        assert!(String::from_utf8_lossy(&тело).contains("нет такого адреса"));
    }
}
