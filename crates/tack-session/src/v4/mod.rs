//! Format-4 JSONL session storage (upstream TS pi's WP06/WP08 storage
//! layer): transaction-log files, branch/lane separation, streaming legacy
//! v3 migration, and policy-driven forks. See `V4_NOTES.md` for the format
//! decisions and deliberate divergences.

pub mod codec;
pub mod fork;
pub mod migrate;
pub mod store;
pub mod types;

pub use codec::{
    ParsedSessionHeader, parse_session_header, parse_transaction, serialize_transaction,
};
pub use fork::run_v4_fork;
pub use migrate::{V4MigrationReport, migrate_v3_to_v4};
pub use store::{V4Error, V4Store};
pub use types::*;
