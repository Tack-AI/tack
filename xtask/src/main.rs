//! xtask: repo automation. Currently the only task is `codegen`:
//! regenerate `crates/tack-ext/src/rpc3.rs` from
//! `protocol/tack-rpc.openrpc.json` (the tack-RPC v3 single source of
//! truth), or verify freshness with `--check` (used by CI).

mod codegen;

use anyhow::Result;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("codegen") => {
            let check = args.any(|arg| arg == "--check");
            codegen::codegen(check)
        }
        _ => {
            eprintln!("usage: cargo run -p xtask -- codegen [--check]");
            std::process::exit(2);
        }
    }
}
