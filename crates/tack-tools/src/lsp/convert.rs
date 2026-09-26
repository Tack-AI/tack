use std::path::{Path, PathBuf};

use serde_json::Value;

/// Percent-encode a filesystem path into a file:// URI.
pub fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s
        .strip_prefix('/')
        .map(|r| r.to_string())
        .unwrap_or_else(|| s.clone());
    let mut out = String::from("file:///");
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    normalize_uri(&out)
}

/// Canonicalize a file:// URI for map keys: lowercase the Windows drive
/// letter (servers disagree on its case, e.g. rust-analyzer publishes
/// `file:///c:/...` regardless of the didOpen casing).
pub(crate) fn normalize_uri(uri: &str) -> String {
    let bytes = uri.as_bytes();
    if bytes.len() >= 10 && uri.starts_with("file:///") && bytes[9] == b':' {
        let mut out = uri.to_string();
        out.replace_range(8..9, &uri[8..9].to_lowercase());
        return out;
    }
    uri.to_string()
}

pub(crate) fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file:///")?;
    let mut bytes = Vec::new();
    let rest = rest.as_bytes();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == b'%' && i + 2 < rest.len() {
            let hex = std::str::from_utf8(&rest[i + 1..i + 3]).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(rest[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(bytes).ok()?;
    #[cfg(windows)]
    {
        Some(PathBuf::from(s.replace('/', "\\")))
    }
    #[cfg(not(windows))]
    {
        Some(PathBuf::from(format!("/{s}")))
    }
}

// ---------------------------------------------------------------------
// Navigation (definition/references/symbols/rename)
// ---------------------------------------------------------------------

/// A resolved source location (1-based line/column for display).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
}

/// A flattened document symbol (depth encodes the hierarchy).
#[derive(Clone, Debug)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub depth: usize,
}

/// Convert 1-based (line, column) to LSP's 0-based (line, UTF-16
/// code-unit offset). Columns are 1-based UTF-16 code units — the same
/// units the server reports in diagnostics and locations, so reported
/// positions round-trip exactly even when the line contains non-BMP
/// characters (emoji = 2 units). The column is clamped to the line's
/// UTF-16 length so a stale column can't send an out-of-range position.
pub(crate) fn to_lsp_position(text: &str, line1: u32, col1: u32) -> (u32, u32) {
    let line0 = line1.saturating_sub(1);
    let col0 = col1.saturating_sub(1) as usize;
    let line_text = text.lines().nth(line0 as usize).unwrap_or("");
    let line_units: usize = line_text.chars().map(|c| c.len_utf16()).sum();
    (line0, col0.min(line_units) as u32)
}

/// Convert an LSP 0-based (line, UTF-16 column) to a byte offset in `text`.
pub(crate) fn lsp_to_offset(text: &str, line0: u32, utf16_col: u32) -> usize {
    let mut offset = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        if i == line0 as usize {
            let mut units = 0usize;
            for (byte_idx, ch) in line.char_indices() {
                if units >= utf16_col as usize {
                    return offset + byte_idx;
                }
                units += ch.len_utf16();
            }
            return offset + line.trim_end_matches('\r').len();
        }
        offset += line.len() + 1;
    }
    text.len()
}

/// Parse a Location | Location[] | LocationLink[] response.
pub(crate) fn parse_locations(value: &Value) -> Vec<Location> {
    let mut out = Vec::new();
    let parse_one = |loc: &Value, out: &mut Vec<Location>| {
        // LocationLink uses targetUri + targetSelectionRange/targetRange.
        let (uri, range) = if loc.get("targetUri").is_some() {
            let range = loc
                .get("targetSelectionRange")
                .or_else(|| loc.get("targetRange"));
            (
                loc["targetUri"].as_str().unwrap_or_default().to_string(),
                range.cloned().unwrap_or(Value::Null),
            )
        } else {
            (
                loc["uri"].as_str().unwrap_or_default().to_string(),
                loc["range"].clone(),
            )
        };
        let Some(path) = uri_to_path(&uri) else {
            return;
        };
        let line = range["start"]["line"].as_u64().unwrap_or(0) as u32;
        let character = range["start"]["character"].as_u64().unwrap_or(0) as u32;
        out.push(Location {
            path,
            line: line + 1,
            column: character + 1,
        });
    };
    match value {
        Value::Array(list) => {
            for loc in list {
                parse_one(loc, &mut out);
            }
        }
        Value::Null => {}
        single => parse_one(single, &mut out),
    }
    out
}

/// LSP SymbolKind → human label.
fn symbol_kind_label(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum-member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type-parameter",
        _ => "symbol",
    }
}

/// A workspace-wide symbol hit (`workspace/symbol`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceSymbol {
    pub name: String,
    pub kind: String,
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
}

/// One caller (incoming) or callee (outgoing) from callHierarchy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSite {
    pub name: String,
    pub kind: String,
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
    /// 1-based lines of the individual call sites inside the item.
    pub call_lines: Vec<u32>,
}

/// Parse a `callHierarchy/incomingCalls` or `.../outgoingCalls` response.
pub(crate) fn parse_call_sites(value: &Value, incoming: bool) -> Vec<CallSite> {
    let mut out = Vec::new();
    for call in value.as_array().into_iter().flatten() {
        let who = if incoming { &call["from"] } else { &call["to"] };
        let Some(path) = uri_to_path(who["uri"].as_str().unwrap_or_default()) else {
            continue;
        };
        let range = if who.get("selectionRange").is_some() {
            &who["selectionRange"]
        } else {
            &who["range"]
        };
        let call_lines = call["fromRanges"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| r["start"]["line"].as_u64().unwrap_or(0) as u32 + 1)
            .collect();
        out.push(CallSite {
            name: who["name"].as_str().unwrap_or_default().to_string(),
            kind: symbol_kind_label(who["kind"].as_u64().unwrap_or(0)).to_string(),
            path,
            line: range["start"]["line"].as_u64().unwrap_or(0) as u32 + 1,
            column: range["start"]["character"].as_u64().unwrap_or(0) as u32 + 1,
            call_lines,
        });
    }
    out
}

/// Filter `workspace/symbol` hits to exact name matches (case-sensitive
/// first, then case-insensitive) for name-based navigation.
pub(crate) fn exact_name_matches(
    symbols: Vec<WorkspaceSymbol>,
    name: &str,
) -> Vec<WorkspaceSymbol> {
    let exact: Vec<WorkspaceSymbol> = symbols.iter().filter(|s| s.name == name).cloned().collect();
    if !exact.is_empty() {
        return exact;
    }
    symbols
        .into_iter()
        .filter(|s| s.name.eq_ignore_ascii_case(name))
        .collect()
}

/// Parse a `workspace/symbol` SymbolInformation[] response. Entries without
/// a resolvable location are skipped.
pub(crate) fn parse_workspace_symbols(value: &Value) -> Vec<WorkspaceSymbol> {
    let mut out = Vec::new();
    for sym in value.as_array().into_iter().flatten() {
        let Some(path) = uri_to_path(sym["location"]["uri"].as_str().unwrap_or_default()) else {
            continue;
        };
        out.push(WorkspaceSymbol {
            name: sym["name"].as_str().unwrap_or_default().to_string(),
            kind: symbol_kind_label(sym["kind"].as_u64().unwrap_or(0)).to_string(),
            path,
            line: sym["location"]["range"]["start"]["line"]
                .as_u64()
                .unwrap_or(0) as u32
                + 1,
            column: sym["location"]["range"]["start"]["character"]
                .as_u64()
                .unwrap_or(0) as u32
                + 1,
        });
    }
    out
}

/// Format a `textDocument/hover` result: `MarkupContent | MarkedString |
/// MarkedString[]`. Markdown passes through; language-tagged strings become
/// fenced code blocks. None when there is nothing to show.
pub(crate) fn format_hover(value: &Value, max_chars: usize) -> Option<String> {
    let contents = &value["contents"];
    let mut parts: Vec<String> = Vec::new();
    let push_marked = |part: &Value, parts: &mut Vec<String>| {
        if let Some(s) = part.as_str() {
            if !s.trim().is_empty() {
                parts.push(s.to_string());
            }
        } else if let Some(v) = part["value"].as_str()
            && !v.trim().is_empty()
        {
            match part["language"].as_str() {
                Some(lang) => parts.push(format!("```{lang}\n{v}\n```")),
                None => parts.push(v.to_string()),
            }
        }
    };
    if contents.get("kind").is_some() {
        // MarkupContent { kind, value }.
        if let Some(v) = contents["value"].as_str()
            && !v.trim().is_empty()
        {
            parts.push(v.to_string());
        }
    } else if let Some(list) = contents.as_array() {
        for part in list {
            push_marked(part, &mut parts);
        }
    } else {
        push_marked(contents, &mut parts);
    }
    let text = parts.join("\n\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(truncate_chars(&text, max_chars))
}

/// Hard character-count truncation with an ellipsis marker.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}\n… (truncated)")
}

/// Filter raw published diagnostics down to those whose range contains the
/// (line, UTF-16 character) position (inclusive on both ends). Used to
/// scope codeAction context to the diagnostic under the cursor.
pub(crate) fn diagnostics_containing(raw: &[Value], line: u32, character: u32) -> Vec<Value> {
    raw.iter()
        .filter(|d| {
            let start = (
                d["range"]["start"]["line"].as_u64().unwrap_or(0) as u32,
                d["range"]["start"]["character"].as_u64().unwrap_or(0) as u32,
            );
            let end = (
                d["range"]["end"]["line"].as_u64().unwrap_or(0) as u32,
                d["range"]["end"]["character"].as_u64().unwrap_or(0) as u32,
            );
            start <= (line, character) && (line, character) <= end
        })
        .cloned()
        .collect()
}

pub(crate) fn flatten_document_symbols(symbols: &[Value], depth: usize, out: &mut Vec<SymbolInfo>) {
    for sym in symbols {
        let name = sym["name"].as_str().unwrap_or("").to_string();
        let kind = symbol_kind_label(sym["kind"].as_u64().unwrap_or(0)).to_string();
        let range = if sym.get("location").is_some() {
            sym["location"]["range"].clone()
        } else {
            sym["range"].clone()
        };
        let line = range["start"]["line"].as_u64().unwrap_or(0) as u32 + 1;
        out.push(SymbolInfo {
            name,
            kind,
            line,
            depth,
        });
        if let Some(children) = sym["children"].as_array() {
            flatten_document_symbols(children, depth + 1, out);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn uri_round_trip() {
        let path = PathBuf::from(if cfg!(windows) {
            "C:\\work dir\\a.rs"
        } else {
            "/work dir/a.rs"
        });
        let uri = path_to_uri(&path);
        assert!(uri.starts_with("file:///"));
        assert!(uri.contains("%20"));
        let back = uri_to_path(&uri).unwrap();
        assert_eq!(back, path);
    }

    #[test]
    fn call_sites_parse_incoming_and_outgoing() {
        let incoming = json!([{
            "from": {
                "name": "handle_agent_event",
                "kind": 12,
                "uri": "file:///tmp/app.rs",
                "range": { "start": { "line": 100, "character": 0 }, "end": { "line": 110, "character": 0 } },
                "selectionRange": { "start": { "line": 101, "character": 9 }, "end": { "line": 101, "character": 28 } }
            },
            "fromRanges": [
                { "start": { "line": 104, "character": 20 }, "end": { "line": 104, "character": 40 } }
            ]
        }]);
        let calls = parse_call_sites(&incoming, true);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "handle_agent_event");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].line, 102); // selectionRange, 1-based
        assert_eq!(calls[0].call_lines, vec![105]);

        let outgoing = json!([{
            "to": {
                "name": "mark_queued_delivered",
                "kind": 6,
                "uri": "file:///tmp/app.rs",
                "range": { "start": { "line": 50, "character": 0 }, "end": { "line": 60, "character": 0 } }
            },
            "fromRanges": []
        }]);
        let calls = parse_call_sites(&outgoing, false);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "mark_queued_delivered");
        assert_eq!(calls[0].kind, "method");
        assert_eq!(calls[0].line, 51); // falls back to range
    }

    #[test]
    fn exact_name_matches_prefers_case_sensitive() {
        let sym = |name: &str| WorkspaceSymbol {
            name: name.to_string(),
            kind: "function".to_string(),
            path: PathBuf::from("/tmp/a.rs"),
            line: 1,
            column: 1,
        };
        let all = vec![sym("rename"), sym("Rename"), sym("rename_all")];
        assert_eq!(exact_name_matches(all.clone(), "rename").len(), 1);
        assert_eq!(exact_name_matches(all.clone(), "RENAME").len(), 2);
        assert!(exact_name_matches(all, "missing").is_empty());
    }

    #[test]
    fn hover_formatting_shapes() {
        // Plain string.
        assert_eq!(
            format_hover(&json!({ "contents": "fn main() {}" }), 100).as_deref(),
            Some("fn main() {}")
        );
        // MarkupContent passes through.
        let markup = json!({ "contents": { "kind": "markdown", "value": "```rust\ni32\n```" } });
        assert_eq!(
            format_hover(&markup, 100).as_deref(),
            Some("```rust\ni32\n```")
        );
        // MarkedString[]: language-tagged strings become fenced blocks.
        let marked = json!({ "contents": [
            { "language": "rust", "value": "let x: i32" },
            "plain note"
        ] });
        let formatted = format_hover(&marked, 100).unwrap();
        assert!(
            formatted.contains("```rust\nlet x: i32\n```"),
            "{formatted}"
        );
        assert!(formatted.contains("plain note"));
        // Empty content → None.
        assert_eq!(format_hover(&json!({ "contents": "  " }), 100), None);
        assert_eq!(format_hover(&Value::Null, 100), None);
        // Truncation.
        let truncated = format_hover(&json!({ "contents": "x".repeat(500) }), 100).unwrap();
        assert!(truncated.contains("truncated"));
        assert!(truncated.chars().count() < 500);
    }

    #[test]
    fn workspace_symbols_parsing() {
        let value = json!([
            { "name": "main", "kind": 12, "location": { "uri": "file:///c%3A/work/a.rs", "range": { "start": { "line": 3, "character": 0 } } } },
            { "name": "no-location" },
            { "name": "helper", "kind": 13, "location": { "uri": "file:///c%3A/work/b.rs", "range": { "start": { "line": 10, "character": 4 } } } }
        ]);
        let symbols = parse_workspace_symbols(&value);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "main");
        assert_eq!(symbols[0].line, 4);
        assert_eq!(symbols[0].column, 1);
        assert_eq!(symbols[0].kind, "function");
        assert!(symbols[1].path.to_string_lossy().ends_with("b.rs"));
        assert_eq!(symbols[1].kind, "variable");
        // Non-array result (e.g. null) → empty.
        assert!(parse_workspace_symbols(&Value::Null).is_empty());
    }

    #[test]
    fn diagnostics_containing_filters_by_range() {
        let raw = vec![
            json!({ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 5 } } }),
            json!({ "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 3, "character": 10 } } }),
        ];
        assert_eq!(diagnostics_containing(&raw, 0, 3).len(), 1);
        assert_eq!(diagnostics_containing(&raw, 2, 7).len(), 1);
        assert_eq!(diagnostics_containing(&raw, 1, 0).len(), 0);
        // Range ends are inclusive.
        assert_eq!(diagnostics_containing(&raw, 3, 10).len(), 1);
    }

    #[test]
    fn position_conversion_handles_utf16() {
        // Columns are UTF-16 code units, matching what the server reports in
        // diagnostics/locations — so a reported position round-trips exactly.
        let text = "let x = \"a😀b\";\nfoo(x);";
        // 'b' is the 12th char but the 13th UTF-16 unit (emoji = 2 units):
        // a server reports it at column 13, and that must resolve back to 'b'.
        let (line, col) = to_lsp_position(text, 1, 13);
        assert_eq!(line, 0);
        assert_eq!(col, 12);
        // Round-trip: LSP offset → byte offset (emoji = 4 bytes, so 'b' is
        // at byte 14 even though its UTF-16 column is 12).
        let off = lsp_to_offset(text, 0, col);
        assert_eq!(&text[off..off + 1], "b");
        // Out-of-range columns clamp to the line's UTF-16 length instead of
        // sending a position past the end of the line.
        let (line, col) = to_lsp_position(text, 2, 999);
        assert_eq!((line, col), (1, 7));
        // Second line.
        let off2 = lsp_to_offset(text, 1, 4);
        assert_eq!(&text[off2..off2 + 1], "x");
    }

    /// Regression: user-facing columns must be UTF-16 units — previously
    /// `to_lsp_position` counted *chars*, so feeding a server-reported
    /// column back (hover at a reported location) landed left of the target
    /// whenever a non-BMP character preceded it on the line.
    #[test]
    fn reported_utf16_columns_round_trip() {
        // `😀` on the line shifts UTF-16 columns after it by +1 vs chars.
        let text = "😀 let target = 1;";
        // Server reports `target` at UTF-16 col 7 (0-based): 2 (emoji) + 5.
        let reported_col1 = 8; // 1-based
        let (line, character) = to_lsp_position(text, 1, reported_col1);
        assert_eq!((line, character), (0, 7));
        let off = lsp_to_offset(text, line, character);
        assert!(text[off..].starts_with("target"), "{}", &text[off..]);
    }

    #[test]
    fn parse_locations_all_shapes() {
        let single = json!({ "uri": "file:///c%3A/work/a.rs", "range": { "start": { "line": 4, "character": 2 }, "end": { "line": 4, "character": 5 } } });
        let locs = parse_locations(&single);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].line, 5);
        let link = json!([{ "targetUri": "file:///c%3A/work/b.rs", "targetSelectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } } }]);
        let locs = parse_locations(&link);
        assert_eq!(locs.len(), 1);
        assert!(locs[0].path.to_string_lossy().ends_with("b.rs"));
        assert!(parse_locations(&Value::Null).is_empty());
    }
}
