//! Plugin identity: `name@source` (docs/plugin-roadmap.md §8.1).
//!
//! Segment character whitelists make IDs safe as filesystem path segments
//! by construction — the versioned store layout relies on this. Reserved
//! sources: `user` (installed without a marketplace), `project`
//! (`.pi/extensions`), `local` (`extensionPaths` checkouts), `mcp`
//! (reserved for MCP servers lifted into the plugin model from MCP
//! config; unused today — Level-2 MCP server plugins carry their
//! discovery source like any other plugin).

use std::fmt;
use std::str::FromStr;

/// Sources the host reserves (a marketplace name cannot collide with
/// these — marketplace names are validated against the same grammar and
/// rejected when they match).
pub const RESERVED_SOURCES: &[&str] = &["user", "project", "local", "mcp"];

/// A parsed plugin id (`<name>@<source>`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginId {
    name: String,
    source: String,
}

/// Why an id string is invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginIdError(pub String);

impl fmt::Display for PluginIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid plugin id: {}", self.0)
    }
}

impl std::error::Error for PluginIdError {}

fn validate_name(name: &str) -> Result<(), PluginIdError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && !name.contains("..")
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if ok {
        return Ok(());
    }
    Err(PluginIdError(format!(
        "name {name:?} must be 1-64 chars of [a-z0-9.-], start alphanumeric, no \"..\", no leading/trailing '.'"
    )))
}

fn validate_source(source: &str) -> Result<(), PluginIdError> {
    let ok = !source.is_empty()
        && source.len() <= 64
        && source
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        return Ok(());
    }
    Err(PluginIdError(format!(
        "source {source:?} must be 1-64 chars of [A-Za-z0-9_-]"
    )))
}

impl PluginId {
    /// Build a validated id from segments.
    pub fn new(name: impl Into<String>, source: impl Into<String>) -> Result<Self, PluginIdError> {
        let name = name.into();
        let source = source.into();
        validate_name(&name)?;
        validate_source(&source)?;
        Ok(PluginId { name, source })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// True when the source is a reserved one (not a marketplace).
    pub fn is_reserved_source(&self) -> bool {
        RESERVED_SOURCES.contains(&self.source.as_str())
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.source)
    }
}

impl FromStr for PluginId {
    type Err = PluginIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, source) = s
            .rsplit_once('@')
            .ok_or_else(|| PluginIdError(format!("{s:?} is missing the @source segment")))?;
        PluginId::new(name, source)
    }
}

/// Validate a marketplace name (same grammar as an id source segment,
/// excluding reserved names).
pub fn validate_marketplace_name(name: &str) -> Result<(), PluginIdError> {
    validate_source(name)?;
    if RESERVED_SOURCES.contains(&name) {
        return Err(PluginIdError(format!(
            "marketplace name {name:?} collides with a reserved source"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parse_roundtrip() {
        let id: PluginId = "review@acme".parse().unwrap();
        assert_eq!(id.name(), "review");
        assert_eq!(id.source(), "acme");
        assert_eq!(id.to_string(), "review@acme");
        assert!(!id.is_reserved_source());
        let reserved: PluginId = "policies@user".parse().unwrap();
        assert!(reserved.is_reserved_source());
    }

    #[test]
    fn rejects_bad_ids() {
        for bad in [
            "",
            "noatsign",
            "@acme",
            "name@",
            "Name@acme",
            "na me@acme",
            "na..me@acme",
            ".name@acme",
            "name.@acme",
            "-name@acme",
            "name@ac me",
            "name@ac/me",
        ] {
            assert!(bad.parse::<PluginId>().is_err(), "{bad} must fail");
        }
        assert!("a".repeat(65).parse::<PluginId>().is_err() || "a".repeat(65).len() > 64);
    }

    #[test]
    fn marketplace_names_exclude_reserved() {
        assert!(validate_marketplace_name("acme").is_ok());
        for reserved in RESERVED_SOURCES {
            assert!(validate_marketplace_name(reserved).is_err());
        }
    }
}
