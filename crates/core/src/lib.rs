//! The codecraft engine: sessions, the agent loop, approvals, sub-agents,
//! compaction, config and project context.

pub mod agents;
pub mod approval;
pub mod compact;
pub mod config;
pub mod context;
pub mod engine;
pub mod persist;
pub mod prompt;
pub mod runner;
pub mod task;

pub use engine::{Agent, Session, SessionSetup};
