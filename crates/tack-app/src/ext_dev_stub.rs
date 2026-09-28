//! ext_dev stub for builds without the `ext` feature: the v3 dev
//! tooling subcommands all bail with the same "no extension support"
//! message as the rest of the extension surface.

use std::path::Path;

use anyhow::Result;

fn unsupported<T>() -> Result<T> {
    anyhow::bail!("tack was built without extension support (feature `ext` disabled)")
}

pub async fn cmd_ext_inspect(_dir: &Path) -> Result<()> {
    unsupported()
}

pub async fn cmd_ext_dev(_dir: &Path, _scenario: Option<&Path>) -> Result<()> {
    unsupported()
}

pub async fn cmd_ext_test(_dir: &Path, _scenario: Option<&Path>) -> Result<()> {
    unsupported()
}

pub fn cmd_ext_new(_dir: &Path, _lang: &str) -> Result<()> {
    unsupported()
}
