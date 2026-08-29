//! Протокол MCP: JSON-RPC 2.0 поверх stdio.
//!
//! Модуль отвечает только за конверт — разбор запроса, форму ответа, коды
//! ошибок. Что именно делают инструменты, он не знает: это `tools`.
//!
//! # Почему свой разбор, а не готовый крейт
//!
//! Нужны три метода (`initialize`, `tools/list`, `tools/call`) и один
//! транспорт. Зависимость ради этого стоила бы дороже, чем сто строк разбора,
//! и привязала бы версию протокола к чужому графику релизов.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Версия протокола, о которой сервер договаривается с клиентом.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Входящий запрос JSON-RPC.
///
/// `id` отсутствует у уведомлений (notifications) — на них ответ не шлётся.
#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Коды ошибок JSON-RPC 2.0, которые сервер действительно возвращает.
///
/// Список короче стандартного намеренно: `InvalidRequest` (-32600) и
/// `InternalError` (-32603) сюда не входят, потому что ни один путь их не
/// отдаёт. Заведённый «на будущее» вариант перечисления — обещание, которого
/// код не выполняет: он выглядит как поддержанный случай и не является им.
/// Понадобятся — добавляются вместе с местом, которое их возвращает.
#[derive(Debug, Clone, Copy, Serialize)]
#[repr(i32)]
pub enum ErrorCode {
    ParseError = -32700,
    MethodNotFound = -32601,
    InvalidParams = -32602,
}

/// Успешный ответ на запрос с данным `id`.
pub fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Ответ-ошибка уровня протокола.
///
/// Важно отличать от ошибки инструмента: неверный SQL — это результат вызова
/// с `isError: true`, а не поломка протокола. Смешивать нельзя, иначе клиент
/// не отличит «сервер сломался» от «запрос не удался».
pub fn err(id: Value, code: ErrorCode, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code as i32, "message": message.into()}
    })
}

/// Ответ на `initialize`: чем сервер представляется клиенту.
pub fn initialize_result(server_name: &str, version: &str) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": server_name, "version": version}
    })
}

/// Результат вызова инструмента: текстовое содержимое.
///
/// `is_error = true` означает «инструмент отработал и говорит, что не вышло».
/// Это НЕ ошибка протокола — см. `err`.
pub fn tool_result(text: String, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn разбирает_запрос_с_параметрами() {
        let r: Request = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x"}}"#,
        )
        .unwrap();
        assert_eq!(r.method, "tools/call");
        assert_eq!(r.id, Some(json!(1)));
        assert_eq!(r.params["name"], "x");
    }

    #[test]
    fn уведомление_без_id_разбирается() {
        // Уведомления (notifications) не несут id и не требуют ответа.
        let r: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(r.id.is_none());
    }

    #[test]
    fn ошибка_инструмента_не_ошибка_протокола() {
        // Ключевое различие: результат с isError — успешный ответ JSON-RPC.
        let res = ok(json!(1), tool_result("не вышло".into(), true));
        assert!(res.get("error").is_none(), "это не ошибка протокола");
        assert_eq!(res["result"]["isError"], true);
    }

    #[test]
    fn коды_ошибок_соответствуют_стандарту() {
        assert_eq!(ErrorCode::MethodNotFound as i32, -32601);
        assert_eq!(ErrorCode::InvalidParams as i32, -32602);
    }
}
