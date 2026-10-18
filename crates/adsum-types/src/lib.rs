//! Core wire types for Adsum: ids, activity commands, bridge identity.
//!
//! Pure data + validation: no I/O, no threads, no globals. Every public
//! type documents its wire format; [`AppId`] and [`SocketId`] are `Arc<str>`
//! newtypes so hot-path clones and map lookups never allocate.

pub mod app_id;
pub mod cmd;
pub mod detect_source;
pub mod user;

pub use app_id::{AppId, SocketId};
pub use detect_source::DetectSource;
