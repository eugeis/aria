//! eugeis-zeroclaw — ZeroClaw gateway WebSocket client (chat turns + approvals).

pub mod client;
pub mod protocol;

pub use client::{
    AgentError, AgentEvent, ApprovalInfo, Decision, GatewayConfig, TurnOutcome, ZeroClaw,
};
pub use protocol::InFrame;
