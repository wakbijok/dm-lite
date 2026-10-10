//! dm-lite as a library: the typed memory store, hybrid recall, MCP surface and
//! server, for the `dmem` binary and for sibling products that embed the same core
//! (daimon-rag builds its knowledge engine on it). Public module tree, no behaviour
//! of its own; `main.rs` is the CLI over these modules.

pub mod bootstrap;
#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "client")]
pub mod migrate;
pub mod config;
pub mod embedder;
pub mod entry;
pub mod hooks;
pub mod mcp;
pub mod metrics;
pub mod render;
pub mod sqlite;
pub mod store;
pub mod tools;
pub mod skills;
#[cfg(feature = "server")]
pub mod iam;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
pub mod service;
#[cfg(feature = "wizard")]
pub mod setup;
#[cfg(any(feature = "server", feature = "client", feature = "self-update"))]
pub mod tls;
#[cfg(feature = "self-update")]
pub mod upgrade;
#[cfg(feature = "zvec")]
pub mod zvec_index;
#[cfg(feature = "ui")]
pub mod ui;

