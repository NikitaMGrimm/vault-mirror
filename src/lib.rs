//! The library behind the `vault-mirror` command. It is published only so
//! that the command can be built; it has no stable API, and any release may
//! change it.

pub mod api;
pub mod app;
pub mod backup;
pub mod config;
pub mod crypto;
pub mod mirror;
pub mod node;
pub mod plan;
pub mod restore;
pub mod schedule;
pub mod snapshot;
pub mod store;
pub mod text;
pub mod vault;
