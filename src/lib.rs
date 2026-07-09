//! Library half of the `vm` tool: everything except CLI parsing/dispatch,
//! so integration tests can drive the machinery directly.

pub mod commands;
pub mod config;
pub mod deploy;
pub mod doctor;
pub mod exec;
pub mod mapping;
pub mod prl;
pub mod proto;
pub mod ssh;
pub mod sync;
