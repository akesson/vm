//! Library half of the `vm` tool: everything except CLI parsing/dispatch,
//! so integration tests can drive the machinery directly.

pub mod claude;
pub mod commands;
pub mod config;
pub mod deploy;
pub mod doctor;
pub mod exec;
pub mod exit;
pub mod guest_env;
pub mod idle;
pub mod journal;
pub mod lock;
pub mod mapping;
pub mod prl;
pub mod prldnd;
pub mod proto;
pub mod reap;
pub mod ssh;
pub mod sync;
