//! `/changelog` support: parse the embedded CHANGELOG.md and surface new
//! entries after upgrades (port of `utils/changelog.ts` semantics; link
//! normalization is unnecessary since tack uses absolute links only).

use std::path::Path;

/// The repository changelog, embedded at build time.
pub const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub content: String,
}

impl ChangelogEntry {
    pub fn version(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parse `## [x.y.z]` sections (TS parseChangelog).
pub fn parse_changelog(content: &str) -> Vec<ChangelogEntry> {
    let mut entries = Vec::new();
    let mut current_lines: Vec<&str> = Vec::new();
    let mut current_version: Option<(u32, u32, u32)> = None;

    for line in content.lines() {
        if line.starts_with("## ") {
            if let Some((major, minor, patch)) = current_version
                && !current_lines.is_empty()
            {
                entries.push(ChangelogEntry {
                    major,
                    minor,
                    patch,
                    content: current_lines.join("\n").trim().to_string(),
                });
            }
            current_version = parse_version_header(line);
            current_lines = if current_version.is_some() {
                vec![line]
            } else {
                Vec::new()
            };
        } else if current_version.is_some() {
            current_lines.push(line);
        }
    }
    if let Some((major, minor, patch)) = current_version
        && !current_lines.is_empty()
    {
        entries.push(ChangelogEntry {
            major,
            minor,
            patch,
            content: current_lines.join("\n").trim().to_string(),
        });
    }
    entries
}

/// `## [1.2.3]` or `## 1.2.3 ...` → (1, 2, 3).
fn parse_version_header(line: &str) -> Option<(u32, u32, u32)> {
    let rest = line.strip_prefix("## ")?.trim_start_matches('[');
    let mut parts = rest.split(['.', ']', ' ', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Entries strictly newer than `last_version` ("x.y.z").
pub fn new_entries_since<'a>(
    entries: &'a [ChangelogEntry],
    last_version: &str,
) -> Vec<&'a ChangelogEntry> {
    let mut parts = last_version
        .split('.')
        .map(|p| p.parse::<u32>().unwrap_or(0));
    let last = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    entries
        .iter()
        .filter(|e| (e.major, e.minor, e.patch) > last)
        .collect()
}

/// Startup "what's new" logic (TS getChangelogForDisplay): fresh installs
/// record the current version silently; upgrades get the new entries' markdown
/// once. Returns `None` when there is nothing to show.
pub fn startup_markdown(agent_dir: &Path) -> Option<String> {
    let last_version = std::fs::read_to_string(agent_dir.join("settings.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|v| v.get("lastChangelogVersion")?.as_str().map(str::to_string));
    let current = env!("CARGO_PKG_VERSION");

    let Some(last_version) = last_version else {
        let _ = crate::settings::Settings::save_global(
            agent_dir,
            "lastChangelogVersion",
            serde_json::Value::String(current.to_string()),
        );
        return None;
    };
    if last_version == current {
        return None;
    }
    let entries = parse_changelog(CHANGELOG);
    let new = new_entries_since(&entries, &last_version);
    if new.is_empty() {
        return None;
    }
    let _ = crate::settings::Settings::save_global(
        agent_dir,
        "lastChangelogVersion",
        serde_json::Value::String(current.to_string()),
    );
    // collapseChangelog: one-line notice; full entries via /changelog.
    let collapse = std::fs::read_to_string(agent_dir.join("settings.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|v| v.get("collapseChangelog")?.as_bool())
        .unwrap_or(false);
    if collapse {
        return Some(format!(
            "Updated to v{current}. Use /changelog to view the full changelog."
        ));
    }
    Some(
        std::iter::once(format!("**Updated to v{current}** — what's new:"))
            .chain(new.iter().map(|e| e.content.clone()))
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parses_entries() {
        let md = "# Title\n\n## [0.2.0]\n\n- b\n\n## [0.1.0]\n\n- a\n";
        let entries = parse_changelog(md);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].version(), "0.2.0");
        assert!(entries[0].content.contains("- b"));
        assert!(entries[1].content.contains("- a"));
    }

    #[test]
    fn filters_newer_than_last() {
        let md = "## [0.2.0]\n\n- b\n\n## [0.1.5]\n\n- a5\n\n## [0.1.0]\n\n- a\n";
        let entries = parse_changelog(md);
        let new = new_entries_since(&entries, "0.1.0");
        assert_eq!(new.len(), 2);
        assert!(new_entries_since(&entries, "0.2.0").is_empty());
        assert_eq!(new_entries_since(&entries, "0.0.0").len(), 3);
    }

    #[test]
    fn embedded_changelog_parses() {
        let entries = parse_changelog(CHANGELOG);
        assert!(
            !entries.is_empty(),
            "CHANGELOG.md must have version entries"
        );
        assert_eq!(entries[0].version(), env!("CARGO_PKG_VERSION"));
    }
}
