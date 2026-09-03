//! Библиотечная часть справочно-навигационного сервера по 1С.
//!
//! Модули объявлены здесь, а не только в `main.rs`, чтобы бизнес-логику
//! инструментов (`tools`, `grep`, `changes`, `sql`) можно было линковать
//! напрямую из другого приложения — без stdio/JSON-RPC обвязки (`server`,
//! `proto`, `http`), когда она не нужна вызывающей стороне.

pub mod autoupdate;
pub mod changes;
pub mod freshness_guard;
pub mod grep;
pub mod hooks;
pub mod http;
pub mod install;
pub mod mcp_http;
pub mod proto;
pub mod registry;
pub mod server;
pub mod sql;
pub mod tools;
pub mod ui;
