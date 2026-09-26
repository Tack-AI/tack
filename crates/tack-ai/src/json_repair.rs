//! Lenient JSON parsing for streamed tool-call arguments.
//! Port of `packages/ai/src/utils/json-parse.ts` (repairJson + partial parse).

use serde_json::Value;

// 'u' is listed for completeness but never matched via this table: the
// dedicated Some('u') arm runs first and validates the four hex digits.
const VALID_JSON_ESCAPES: &[char] = &['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

fn is_control_character(c: char) -> bool {
    ('\u{0}'..='\u{1f}').contains(&c)
}

fn escape_control_character(c: char) -> String {
    match c {
        '\u{8}' => "\\b".to_string(),
        '\u{c}' => "\\f".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        c => format!("\\u{:04x}", c as u32),
    }
}

/// Repair malformed JSON string literals by escaping raw control characters
/// inside strings and doubling backslashes before invalid escape characters.
pub fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut index = 0;

    while index < chars.len() {
        let c = chars[index];

        if !in_string {
            repaired.push(c);
            if c == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }

        if c == '"' {
            repaired.push(c);
            in_string = false;
            index += 1;
            continue;
        }

        if c == '\\' {
            let next = chars.get(index + 1).copied();
            match next {
                None => {
                    repaired.push_str("\\\\");
                }
                Some('u') => {
                    let digits: String = chars.iter().skip(index + 2).take(4).collect();
                    if digits.len() == 4 && digits.chars().all(|d| d.is_ascii_hexdigit()) {
                        repaired.push_str("\\u");
                        repaired.push_str(&digits);
                        // Backslash + u + 4 digits consumed.
                        index += 6;
                        continue;
                    }
                    // Incomplete/invalid \u escape: escape the backslash so
                    // the 'u' (reprocessed next iteration) stays literal.
                    repaired.push_str("\\\\");
                }
                Some(n) if VALID_JSON_ESCAPES.contains(&n) => {
                    repaired.push('\\');
                    repaired.push(n);
                    index += 2;
                    continue;
                }
                Some(_) => {
                    repaired.push_str("\\\\");
                }
            }
            index += 1;
            continue;
        }

        if is_control_character(c) {
            repaired.push_str(&escape_control_character(c));
        } else {
            repaired.push(c);
        }
        index += 1;
    }

    repaired
}

/// Best-effort completion of a truncated JSON document: drops an unterminated
/// string's tail, then closes any open objects/arrays. Used to show partial
/// tool-call arguments mid-stream.
fn complete_truncated(json: &str) -> String {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut out = json.to_string();

    for c in json.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }

    if in_string {
        out.push('"');
    }
    // Trim a trailing half-written value: `{"a": 12` is fine to close, but
    // `{"a": tru` is not — walk back to the last structural character.
    if !stack.is_empty() {
        let trimmed = out.trim_end();
        let cut = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| {
                matches!(c, '{' | '[' | ',' | ':' | '"' | '}' | ']') || c.is_ascii_alphanumeric()
            })
            .map(|(i, _)| i + 1)
            .unwrap_or(0);
        let mut candidate = trimmed[..cut].to_string();
        // A trailing keyword fragment like `tru`/`nul` breaks parsing; drop it.
        for kw in ["true", "false", "null"] {
            for n in 1..kw.len() {
                if candidate.ends_with(&kw[..n]) {
                    candidate.truncate(candidate.len() - n);
                }
            }
        }
        // The truncation above can leave whitespace behind (e.g. `{"a": tru`
        // → `{"a": `), which would defeat the dangling-colon cleanup below.
        candidate = candidate.trim_end().to_string();
        // Drop a dangling `key:` or trailing comma.
        while candidate.ends_with(':') || candidate.ends_with(',') {
            candidate.pop();
            candidate = candidate.trim_end().to_string();
        }
        // Drop a dangling key: the trailing token is a string in key position
        // (its opening quote is preceded by `,` or `{`), e.g. `{"a": 1, "b"`.
        if candidate.ends_with('"') {
            let bytes: Vec<char> = candidate.chars().collect();
            // Find the opening quote of the trailing string.
            let mut idx = bytes.len().saturating_sub(2);
            loop {
                if bytes[idx] == '"' && bytes.get(idx.wrapping_sub(1)) != Some(&'\\') {
                    break;
                }
                if idx == 0 {
                    break;
                }
                idx -= 1;
            }
            let before = bytes[..idx].iter().rev().find(|c| !c.is_whitespace());
            if matches!(before, Some(',') | Some('{')) {
                candidate = bytes[..idx]
                    .iter()
                    .collect::<String>()
                    .trim_end()
                    .to_string();
                if candidate.ends_with(',') {
                    candidate.pop();
                }
            }
        }
        out = candidate;
        while let Some(close) = stack.pop() {
            out.push(close);
        }
    }

    out
}

/// Port of `parseStreamingJson`: full parse, then repaired parse, then a
/// truncated-completion parse; falls back to an empty object.
pub fn parse_streaming_json(partial: &str) -> Value {
    if let Ok(v) = serde_json::from_str(partial) {
        return v;
    }
    let repaired = repair_json(partial);
    if let Ok(v) = serde_json::from_str(&repaired) {
        return v;
    }
    let completed = complete_truncated(&repaired);
    if let Ok(v) = serde_json::from_str(&completed) {
        return v;
    }
    Value::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_complete_json() {
        assert_eq!(parse_streaming_json(r#"{"a": 1}"#), json!({ "a": 1 }));
    }

    #[test]
    fn completes_truncated_json() {
        assert_eq!(
            parse_streaming_json(r#"{"a": [1, 2"#),
            json!({ "a": [1, 2] })
        );
        assert_eq!(
            parse_streaming_json(r#"{"path": "src/ma"#),
            json!({ "path": "src/ma" })
        );
    }

    #[test]
    fn repairs_control_characters() {
        assert_eq!(
            parse_streaming_json("{\"a\": \"x\ny\"}"),
            json!({ "a": "x\ny" })
        );
    }

    #[test]
    fn drops_dangling_key() {
        assert_eq!(parse_streaming_json(r#"{"a": 1, "b""#), json!({ "a": 1 }));
    }

    #[test]
    fn garbage_falls_back_to_empty_object() {
        assert_eq!(parse_streaming_json("{{{{"), json!({}));
    }

    #[test]
    fn drops_truncated_keyword_fragment() {
        // Regression: `tru` truncation left `{"a": 1, "b": ` (trailing
        // space), defeating the dangling-colon cleanup and losing `"a"`.
        assert_eq!(
            parse_streaming_json(r#"{"a": 1, "b": tru"#),
            json!({ "a": 1 })
        );
        assert_eq!(parse_streaming_json(r#"{"a": nul"#), json!({}));
        assert_eq!(
            parse_streaming_json(r#"{"ok": true, "done": fal"#),
            json!({ "ok": true })
        );
    }

    #[test]
    fn keeps_partial_number_and_string_values() {
        assert_eq!(
            parse_streaming_json(r#"{"a": "x", "b": 12"#),
            json!({ "a": "x", "b": 12 })
        );
        assert_eq!(parse_streaming_json(r#"[1, 2,"#), json!([1, 2]));
        assert_eq!(
            parse_streaming_json(r#"{"a": "x", "b": "y"#),
            json!({ "a": "x", "b": "y" })
        );
    }

    #[test]
    fn escaped_quote_at_boundary() {
        // Input text: {"a": "x\" — trailing escaped quote inside the string.
        let input = "{\"a\": \"x\\\"";
        assert_eq!(
            parse_streaming_json(input),
            serde_json::json!({ "a": "x\"" })
        );
        // Input text: {"a": "x\ — lone trailing backslash (doubled by repair).
        let input = "{\"a\": \"x\\";
        assert_eq!(
            parse_streaming_json(input),
            serde_json::json!({ "a": "x\\" })
        );
    }

    #[test]
    fn repair_invalid_escapes_and_unicode() {
        // Invalid escape \y doubles the backslash.
        assert_eq!(repair_json("{\"a\": \"x\\yz\"}"), "{\"a\": \"x\\\\yz\"}");
        // Valid \u escape passes through untouched and parses.
        let unicode = "{\"a\": \"\\u0041b\"}";
        assert_eq!(repair_json(unicode), unicode);
        assert_eq!(
            parse_streaming_json(unicode),
            serde_json::json!({ "a": "Ab" })
        );
    }

    #[test]
    fn repair_invalid_unicode_escape_keeps_json_valid() {
        // Regression: an incomplete \u escape used to emit "\\u" and then
        // push 'u' AGAIN next iteration, producing a guaranteed-invalid
        // "\u005cuu12". The backslash must be escaped instead, keeping the
        // 'u' literal and the surrounding JSON parseable.
        assert_eq!(repair_json("{\"a\": \"\\u12"), "{\"a\": \"\\\\u12");
        assert_eq!(
            parse_streaming_json("{\"a\": \"\\u12"),
            serde_json::json!({ "a": "\\u12" })
        );
        // Non-hex digits after \u: same treatment.
        assert_eq!(
            repair_json("{\"a\": \"\\uzzzz\"}"),
            "{\"a\": \"\\\\uzzzz\"}"
        );
        assert_eq!(
            parse_streaming_json("{\"a\": \"\\uzzzz\"}"),
            serde_json::json!({ "a": "\\uzzzz" })
        );
        // A valid \u escape mid-string is still untouched.
        assert_eq!(repair_json("{\"a\": \"x\\u0041"), "{\"a\": \"x\\u0041");
    }
}
