//! Gofer: a small local action service.
//!
//! External applications call HTTP/CLI endpoints with an optional shared token:
//! - WeChat notification (current open chat)
//! - Xiaohongshu group notification
//! - Maoyan ticket locking via maoyan-booker
//!
//! Requests are persisted in SQLite so the panel can show status, latency and
//! per-capability success counts. The service deliberately does not call back
//! into the caller; callers can poll `/v1/requests/:id`.

pub mod capabilities;
pub mod db;
pub mod lock;
pub mod notify;
pub mod server;
pub mod update;
pub mod web;
