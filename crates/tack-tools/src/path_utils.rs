//! Path resolution helpers. Lean port of
//! `packages/coding-agent/src/core/tools/path-utils.ts` (macOS NFD variant
//! lookup omitted).

use std::path::{Path, PathBuf};

const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

/// Expand `@`-prefixes, `~`, and normalize Unicode spaces, then resolve
/// relative to `cwd`.
pub fn resolve_to_cwd(file_path: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_path(file_path);
    let path = Path::new(&expanded);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Strip a leading `@`, expand `~`, normalize exotic Unicode spaces.
pub fn expand_path(file_path: &str) -> String {
    let mut path = file_path.strip_prefix('@').unwrap_or(file_path).to_string();
    if (path == "~" || path.starts_with("~/") || path.starts_with("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        path = format!("{}{}", home.display(), &path[1..]);
    }
    // Normalize exotic Unicode spaces to regular spaces.
    path.chars()
        .map(|c| match c {
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

fn file_exists(path: &Path) -> bool {
    path.exists()
}

/// Resolve a path for reading, trying macOS screenshot filename variants
/// (narrow no-break space before AM/PM, curly quotes) when the plain path
/// does not exist.
pub fn resolve_read_path(file_path: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(file_path, cwd);
    if file_exists(&resolved) {
        return resolved;
    }
    let s = resolved.to_string_lossy();

    // Narrow no-break space before AM/PM (macOS screenshot names).
    let am_pm = s
        .replace(" AM.", &format!("{NARROW_NO_BREAK_SPACE}AM."))
        .replace(" PM.", &format!("{NARROW_NO_BREAK_SPACE}PM."));
    if am_pm != s && file_exists(Path::new(&am_pm)) {
        return PathBuf::from(am_pm);
    }

    // Curly quotes (macOS localized screenshot names).
    let curly = s.replace('\'', "\u{2019}");
    if curly != s && file_exists(Path::new(&curly)) {
        return PathBuf::from(curly);
    }

    resolved
}
