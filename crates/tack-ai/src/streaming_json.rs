//! Incremental, tolerant JSON previews for streamed tool-call arguments.
//!
//! The parser consumes each delta once and keeps the original text separately.
//! A completed tool call must still use [`crate::json_repair::parse_tool_call_arguments`]:
//! a preview is deliberately allowed to repair or omit incomplete input, but it
//! must never become the validation boundary for tool execution.

use serde_json::{Map, Number, Value};

const CREDIT_PER_APPEND: usize = 256;
const CREDIT_PER_BYTE: usize = 8;
const SNAPSHOT_FIXED_COST: usize = 64;
const MAX_PENDING_SCALAR_LENGTH: usize = 64;
/// Match serde_json's default recursion ceiling. Deeper input is consumed
/// without building a recursively cloned/dropped `Value`; strict finish still
/// sees the complete raw text and rejects it normally.
const MAX_PREVIEW_DEPTH: usize = 128;

/// Where a value belongs in its parent. `None` is a value in an invalid
/// position: the state machine consumes it but does not expose it.
#[derive(Clone, Debug)]
enum Slot {
    Root,
    Member(String),
    Element(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Key,
    Colon,
    Value,
    Comma,
}

/// An open container owns its working value. Closing moves that value into its
/// parent; snapshots clone only the currently open path instead of requiring
/// shared mutable JSON nodes.
#[derive(Debug)]
struct Frame {
    container: Value,
    slot: Option<Slot>,
    key: Option<String>,
    phase: Phase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StringTarget {
    Key,
    Value,
}

#[derive(Debug)]
struct OpenString {
    target: StringTarget,
    slot: Option<Slot>,
    /// Object keys need their own buffer. Value strings live directly in the
    /// working tree so appending text never clones the accumulated string.
    key_text: String,
    /// Empty, a backslash, or an in-progress `\u` escape (at most six bytes).
    escape: String,
    /// Rust strings cannot contain an isolated surrogate. Hold a high half
    /// until its low half arrives; an unmatched half previews as U+FFFD.
    high_surrogate: Option<u16>,
}

#[derive(Debug)]
struct PendingScalar {
    slot: Slot,
    value: Value,
}

/// Stateful tolerant parser. This is private because tool calls need the
/// object-only projection and raw-text lifecycle provided by the wrapper.
#[derive(Debug, Default)]
struct StreamingJsonParser {
    root: Option<Value>,
    done: bool,
    stack: Vec<Frame>,
    string: Option<OpenString>,
    scalar: String,
    scalar_slot: Option<Slot>,
    source_len: usize,
    credit: usize,
    dirty: bool,
}

impl StreamingJsonParser {
    fn append(&mut self, text: &str) {
        self.source_len = self.source_len.saturating_add(text.len());
        self.credit = self.credit.saturating_add(
            CREDIT_PER_APPEND.saturating_add(text.len().saturating_mul(CREDIT_PER_BYTE)),
        );

        let mut index = 0;
        while index < text.len() && !self.done {
            if self.string.is_some() {
                index = self.read_string(text, index);
            } else if !self.scalar.is_empty() {
                index = self.read_scalar(text, index);
            } else {
                let ch = text[index..].chars().next().expect("index is in bounds");
                self.structural(ch);
                index += ch.len_utf8();
            }
        }
    }

    /// Return a fresh owned snapshot, or `None` when the value is unchanged or
    /// the normal copy budget prefers a later snapshot. Final previews bypass
    /// that budget so trailing deltas cannot be stranded at EOF.
    fn snapshot(&mut self, force: bool) -> Option<Value> {
        if !self.dirty {
            return None;
        }
        let cost =
            SNAPSHOT_FIXED_COST.saturating_add(self.source_len.saturating_mul(CREDIT_PER_BYTE));
        if !force && cost > self.credit {
            return None;
        }
        self.credit = self.credit.saturating_sub(cost);
        self.dirty = false;
        self.copy_open_path(self.pending_scalar())
    }

    fn read_string(&mut self, text: &str, start: usize) -> usize {
        let mut index = start;
        while index < text.len() {
            let ch = text[index..].chars().next().expect("index is in bounds");
            let next = index + ch.len_utf8();
            let mut open = self.string.take().expect("caller checked open string");

            if !open.escape.is_empty() && self.continue_escape(&mut open, ch) {
                self.string = Some(open);
                index = next;
                continue;
            }

            match ch {
                '"' => {
                    self.end_string(open);
                    return next;
                }
                '\\' => open.escape.push('\\'),
                _ => self.append_decoded(&mut open, ch.encode_utf8(&mut [0; 4])),
            }
            self.string = Some(open);
            index = next;
        }
        index
    }

    /// Continue an escape. `false` means the character was not part of the
    /// escape and must still be processed as ordinary string content.
    fn continue_escape(&mut self, open: &mut OpenString, ch: char) -> bool {
        if open.escape == "\\" {
            match ch {
                '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' => {
                    open.escape.clear();
                    let decoded = match ch {
                        '"' => '"',
                        '\\' => '\\',
                        '/' => '/',
                        'b' => '\u{0008}',
                        'f' => '\u{000c}',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        _ => unreachable!(),
                    };
                    self.append_decoded(open, decoded.encode_utf8(&mut [0; 4]));
                    return true;
                }
                'u' => {
                    open.escape.push('u');
                    return true;
                }
                _ => {
                    // Invalid escapes retain their backslash, like repair_json.
                    open.escape.clear();
                    self.append_decoded(open, "\\");
                    return false;
                }
            }
        }

        if ch.is_ascii_hexdigit() {
            open.escape.push(ch);
            if open.escape.len() < 6 {
                return true;
            }
            let code = u16::from_str_radix(&open.escape[2..6], 16)
                .expect("four ASCII hex digits always parse");
            open.escape.clear();
            self.append_unicode(open, code);
            return true;
        }

        // A non-hex character terminates an invalid Unicode escape. Keep the
        // escape literal, then process this character normally.
        let literal = std::mem::take(&mut open.escape);
        self.append_decoded(open, &literal);
        false
    }

    fn append_unicode(&mut self, open: &mut OpenString, code: u16) {
        if let Some(high) = open.high_surrogate.take() {
            if (0xdc00..=0xdfff).contains(&code) {
                let scalar = 0x1_0000 + (((high as u32) - 0xd800) << 10) + ((code as u32) - 0xdc00);
                let ch = char::from_u32(scalar).expect("valid surrogate pair");
                self.append_raw(open, ch.encode_utf8(&mut [0; 4]));
            } else {
                self.append_raw(open, "\u{fffd}");
                self.append_unicode(open, code);
            }
        } else if (0xd800..=0xdbff).contains(&code) {
            open.high_surrogate = Some(code);
        } else if (0xdc00..=0xdfff).contains(&code) {
            self.append_raw(open, "\u{fffd}");
        } else {
            let ch = char::from_u32(code as u32).expect("non-surrogate u16 is a scalar");
            self.append_raw(open, ch.encode_utf8(&mut [0; 4]));
        }
    }

    fn append_decoded(&mut self, open: &mut OpenString, text: &str) {
        if open.high_surrogate.take().is_some() {
            self.append_raw(open, "\u{fffd}");
        }
        self.append_raw(open, text);
    }

    fn append_raw(&mut self, open: &mut OpenString, text: &str) {
        match open.target {
            StringTarget::Key => open.key_text.push_str(text),
            StringTarget::Value => self.append_at(open.slot.as_ref(), text),
        }
    }

    fn end_string(&mut self, mut open: OpenString) {
        if open.high_surrogate.take().is_some() {
            self.append_raw(&mut open, "\u{fffd}");
        }
        match open.target {
            StringTarget::Key => {
                if let Some(frame) = self.stack.last_mut() {
                    frame.key = Some(open.key_text);
                    frame.phase = Phase::Colon;
                }
            }
            StringTarget::Value => self.value_ended(),
        }
    }

    fn read_scalar(&mut self, text: &str, start: usize) -> usize {
        let mut index = start;
        while index < text.len() {
            let ch = text[index..].chars().next().expect("index is in bounds");
            if is_delimiter(ch) {
                break;
            }
            self.scalar.push(ch);
            index += ch.len_utf8();
            if self.scalar_slot.is_some() {
                self.dirty = true;
            }
        }
        if index < text.len() {
            self.end_scalar();
        }
        index
    }

    fn structural(&mut self, ch: char) {
        match ch {
            ' ' | '\t' | '\n' | '\r' => {}
            '"' => self.start_string(),
            '{' => self.open_container(true),
            '[' => self.open_container(false),
            '}' | ']' => self.close_container(),
            ':' => {
                if let Some(frame) = self.stack.last_mut()
                    && frame.container.is_object()
                    && frame.phase == Phase::Colon
                {
                    frame.phase = Phase::Value;
                }
            }
            ',' => {
                if let Some(frame) = self.stack.last_mut() {
                    frame.phase = if frame.container.is_array() {
                        Phase::Value
                    } else {
                        Phase::Key
                    };
                }
            }
            _ => {
                self.scalar.push(ch);
                self.scalar_slot = self.value_slot();
                if self.scalar_slot.is_some() {
                    self.dirty = true;
                }
            }
        }
    }

    fn value_slot(&self) -> Option<Slot> {
        if self.stack.len() >= MAX_PREVIEW_DEPTH {
            return None;
        }
        let Some(frame) = self.stack.last() else {
            return self.root.is_none().then_some(Slot::Root);
        };
        if let Value::Array(elements) = &frame.container {
            return Some(Slot::Element(elements.len()));
        }
        if matches!(frame.phase, Phase::Value | Phase::Colon) {
            return frame.key.clone().map(Slot::Member);
        }
        None
    }

    fn start_string(&mut self) {
        if self.stack.last().is_some_and(|frame| {
            frame.container.is_object() && matches!(frame.phase, Phase::Key | Phase::Comma)
        }) {
            self.string = Some(OpenString {
                target: StringTarget::Key,
                slot: None,
                key_text: String::new(),
                escape: String::new(),
                high_surrogate: None,
            });
            return;
        }

        let slot = self.value_slot();
        self.place(slot.clone(), Value::String(String::new()));
        self.string = Some(OpenString {
            target: StringTarget::Value,
            slot,
            key_text: String::new(),
            escape: String::new(),
            high_surrogate: None,
        });
    }

    fn open_container(&mut self, object: bool) {
        let slot = self.value_slot();
        let empty = if object {
            Value::Object(Map::new())
        } else {
            Value::Array(Vec::new())
        };
        self.place(slot.clone(), empty.clone());
        self.stack.push(Frame {
            container: empty,
            slot,
            key: None,
            phase: if object { Phase::Key } else { Phase::Value },
        });
        self.dirty = true;
    }

    fn close_container(&mut self) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        self.place(frame.slot, frame.container);
        self.value_ended();
        self.dirty = true;
    }

    fn end_scalar(&mut self) {
        let token = std::mem::take(&mut self.scalar);
        let slot = self.scalar_slot.take();
        if let Some(value) = scalar_value(&token) {
            self.place(slot, value);
        }
        self.value_ended();
    }

    fn value_ended(&mut self) {
        if let Some(frame) = self.stack.last_mut() {
            frame.phase = Phase::Comma;
        } else if self.root.is_some() {
            self.done = true;
        }
    }

    fn pending_scalar(&self) -> Option<PendingScalar> {
        if self.scalar.is_empty() || self.scalar.len() > MAX_PENDING_SCALAR_LENGTH {
            return None;
        }
        let value = pending_scalar_value(&self.scalar)?;
        let slot = self.scalar_slot.clone()?;
        Some(PendingScalar { slot, value })
    }

    fn place(&mut self, slot: Option<Slot>, value: Value) {
        let Some(slot) = slot else {
            return;
        };
        self.dirty = true;
        match slot {
            Slot::Root => self.root = Some(value),
            Slot::Member(key) => {
                if let Some(frame) = self.stack.last_mut()
                    && let Value::Object(members) = &mut frame.container
                {
                    members.insert(key, value);
                }
            }
            Slot::Element(index) => {
                if let Some(frame) = self.stack.last_mut()
                    && let Value::Array(elements) = &mut frame.container
                {
                    if index == elements.len() {
                        elements.push(value);
                    } else if let Some(element) = elements.get_mut(index) {
                        *element = value;
                    }
                }
            }
        }
    }

    fn append_at(&mut self, slot: Option<&Slot>, text: &str) {
        let Some(slot) = slot else {
            return;
        };
        let value = match slot {
            Slot::Root => self.root.as_mut(),
            Slot::Member(key) => self.stack.last_mut().and_then(|frame| {
                frame
                    .container
                    .as_object_mut()
                    .and_then(|members| members.get_mut(key))
            }),
            Slot::Element(index) => self.stack.last_mut().and_then(|frame| {
                frame
                    .container
                    .as_array_mut()
                    .and_then(|elements| elements.get_mut(*index))
            }),
        };
        if let Some(Value::String(string)) = value {
            string.push_str(text);
            self.dirty = true;
        }
    }

    fn copy_open_path(&self, mut pending: Option<PendingScalar>) -> Option<Value> {
        if self.stack.is_empty() {
            return match pending {
                Some(PendingScalar {
                    slot: Slot::Root,
                    value,
                }) => Some(value),
                _ => self.root.clone(),
            };
        }

        let last = self.stack.len() - 1;
        let mut child: Option<Value> = None;
        let mut child_slot: Option<Slot> = None;
        for (depth, frame) in self.stack.iter().enumerate().rev() {
            let mut copy = frame.container.clone();
            if depth == last
                && let Some(PendingScalar { slot, value }) = pending.take()
            {
                set_slot(&mut copy, slot, value);
            }
            if let (Some(child_value), Some(slot)) = (child, child_slot) {
                set_slot(&mut copy, slot, child_value);
            }
            child = Some(copy);
            child_slot = frame.slot.clone();
        }
        if matches!(child_slot, Some(Slot::Root)) {
            child
        } else {
            self.root.clone()
        }
    }
}

fn set_slot(container: &mut Value, slot: Slot, value: Value) {
    match (container, slot) {
        (Value::Object(members), Slot::Member(key)) => {
            members.insert(key, value);
        }
        (Value::Array(elements), Slot::Element(index)) => {
            if index == elements.len() {
                elements.push(value);
            } else if let Some(element) = elements.get_mut(index) {
                *element = value;
            }
        }
        _ => {}
    }
}

fn scalar_value(token: &str) -> Option<Value> {
    match token {
        "true" => Some(Value::Bool(true)),
        "false" => Some(Value::Bool(false)),
        "null" => Some(Value::Null),
        _ => token.parse::<Number>().ok().map(Value::Number),
    }
}

fn pending_scalar_value(token: &str) -> Option<Value> {
    for (literal, value) in [
        ("true", Value::Bool(true)),
        ("false", Value::Bool(false)),
        ("null", Value::Null),
    ] {
        if literal.starts_with(token) {
            return Some(value);
        }
    }
    if let Some(value) = scalar_value(token) {
        return Some(value);
    }
    let exponent = token.find(['e', 'E'])?;
    if matches!(&token[exponent + 1..], "" | "+" | "-") {
        return token[..exponent].parse::<Number>().ok().map(Value::Number);
    }
    None
}

fn is_delimiter(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\n' | '\r' | ',' | ':' | '{' | '}' | '[' | ']' | '"'
    )
}

/// Result of replacing a stream with a provider's authoritative argument text.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolArgumentsReplacement {
    /// A non-empty missing suffix when the new text extends the old buffer;
    /// `None` for equal text or a non-prefix reset.
    pub delta: Option<String>,
    /// A fresh object-only preview when the visible value changed.
    pub preview: Option<Value>,
}

/// Raw argument text plus incremental tolerant previews for one tool call.
///
/// `push_str` consumes every delta without cloning a snapshot. Call `preview`
/// at ordinary event boundaries and `preview_final` at EOF/error boundaries.
/// `finish` always reparses the untouched raw text with the strict final-arg
/// rules; it never validates the repaired preview.
#[derive(Debug, Default)]
pub struct ToolCallArgumentsStream {
    text: String,
    parser: StreamingJsonParser,
}

impl ToolCallArgumentsStream {
    /// Create an empty argument stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Original, unrepaired argument text received so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether no argument bytes have been received.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Consume one delta without materializing a preview.
    pub fn push_str(&mut self, delta: &str) {
        self.text.push_str(delta);
        self.parser.append(delta);
    }

    /// Consume plain-text tool input without running the JSON state machine.
    /// This is for grammar/custom tools whose single string property is not
    /// JSON; `finish` still retains the same untouched raw text.
    pub fn push_raw(&mut self, delta: &str) {
        self.text.push_str(delta);
    }

    /// Consume one delta and return a budgeted object-only preview.
    pub fn append(&mut self, delta: &str) -> Option<Value> {
        self.push_str(delta);
        self.preview()
    }

    /// Return a fresh object-only preview, or `None` when unchanged or delayed
    /// by the copy budget. Non-object roots project to an empty object.
    pub fn preview(&mut self) -> Option<Value> {
        self.parser.snapshot(false).map(as_tool_arguments)
    }

    /// Return a fresh preview regardless of copy budget. Use this at EOF or
    /// transport-error boundaries so the final visible value includes all raw
    /// deltas. This remains tolerant and is not final execution validation.
    pub fn preview_final(&mut self) -> Option<Value> {
        self.parser.snapshot(true).map(as_tool_arguments)
    }

    /// Replace the buffer with authoritative text. Prefix continuations return
    /// their non-empty missing suffix; other replacements reset all parser
    /// state so no stale container or escape can leak into the new value.
    pub fn replace(&mut self, text: &str) -> ToolArgumentsReplacement {
        if text.starts_with(&self.text) {
            let suffix = &text[self.text.len()..];
            if suffix.is_empty() {
                return ToolArgumentsReplacement {
                    delta: None,
                    preview: self.preview_final(),
                };
            }
            self.push_str(suffix);
            return ToolArgumentsReplacement {
                delta: Some(suffix.to_string()),
                preview: self.preview_final(),
            };
        }

        self.clear();
        self.push_str(text);
        ToolArgumentsReplacement {
            delta: None,
            // Empty/whitespace replacements have no parser value but must
            // still clear the previously published tool arguments.
            preview: Some(
                self.preview_final()
                    .unwrap_or_else(|| Value::Object(Map::new())),
            ),
        }
    }

    /// Strictly parse the untouched raw text for a completed tool call.
    pub fn finish(&self) -> (Value, Option<String>) {
        crate::json_repair::parse_tool_call_arguments(&self.text)
    }

    /// Forget raw text and all incremental state. The stream can be reused.
    pub fn clear(&mut self) {
        self.text.clear();
        self.parser = StreamingJsonParser::default();
    }

    /// Take the raw text and reset all incremental state for reuse.
    pub fn take_text(&mut self) -> String {
        self.parser = StreamingJsonParser::default();
        std::mem::take(&mut self.text)
    }
}

fn as_tool_arguments(value: Value) -> Value {
    if value.is_object() {
        value
    } else {
        Value::Object(Map::new())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::json_repair::parse_streaming_json;

    fn chunks_at_boundaries(text: &str, size: usize) -> Vec<&str> {
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + size).min(text.len());
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == start {
                end = start
                    + text[start..]
                        .chars()
                        .next()
                        .expect("non-empty tail")
                        .len_utf8();
            }
            chunks.push(&text[start..end]);
            start = end;
        }
        chunks
    }

    fn exact_after_chunks(text: &str, size: usize) -> Value {
        let mut stream = ToolCallArgumentsStream::new();
        let mut latest = json!({});
        for (index, chunk) in chunks_at_boundaries(text, size).into_iter().enumerate() {
            stream.push_str(chunk);
            if index % 3 == 0
                && let Some(preview) = stream.preview()
            {
                latest = preview;
            }
        }
        if let Some(preview) = stream.preview_final() {
            latest = preview;
        }
        latest
    }

    #[test]
    fn complete_documents_match_serde_at_many_chunk_sizes() {
        let documents = [
            r#"{"path":"src/main.rs","content":"line 1\nline 2","mode":"write"}"#,
            r#"{"a":[1,-2,3.5e2,true,false,null,{"b":"x"}],"c":{}}"#,
            r#"{"unicode":"αβγ 😀","escaped":"quote: \"; slash: \\; tab:\t"}"#,
            r#"{"duplicate":1,"duplicate":2,"__proto__":{"safe":true}}"#,
        ];
        for document in documents {
            let expected: Value =
                serde_json::from_str(document).expect("test document must be valid JSON");
            for size in [1, 2, 3, 7, 64, 4096] {
                assert_eq!(exact_after_chunks(document, size), expected, "{document}");
            }
        }
    }

    #[test]
    fn every_prefix_matches_legacy_for_stable_preview_subset() {
        let document = r#"{"path":"src/main.rs","items":[1,2,{"name":"αβ"}],"n":123}"#;
        for (index, ch) in document.char_indices() {
            let _ = ch;
            let prefix = &document[..index];
            let actual = exact_after_chunks(prefix, 3);
            assert_eq!(
                actual,
                parse_streaming_json(prefix),
                "prefix diverged at byte {index}: {prefix:?}"
            );
        }
        assert_eq!(
            exact_after_chunks(document, 1),
            parse_streaming_json(document)
        );
    }

    #[test]
    fn complete_malformed_documents_match_legacy_repair() {
        for document in [
            r#"{"note":"\url"}"#,
            "{\"note\":\"x\ny\"}",
            r#"{"text":"\uzzzz"}"#,
        ] {
            for size in [1, 2, 5, 64] {
                assert_eq!(
                    exact_after_chunks(document, size),
                    parse_streaming_json(document),
                    "{document:?} chunk {size}"
                );
            }
        }
    }

    #[test]
    fn escape_and_unicode_splits_preserve_final_text() {
        for (document, expected) in [
            (r#"{"a":"x\u00e9y"}"#, "xéy"),
            (r#"{"a":"x\ny"}"#, "x\ny"),
            (r#"{"a":"x\\y"}"#, "x\\y"),
            (r#"{"a":"x\"y"}"#, "x\"y"),
            (r#"{"a":"\ud83d\ude00!"}"#, "😀!"),
        ] {
            for split in 0..=document.len() {
                if !document.is_char_boundary(split) {
                    continue;
                }
                let mut stream = ToolCallArgumentsStream::new();
                stream.push_str(&document[..split]);
                let mut latest = stream.preview_final().unwrap_or_else(|| json!({}));
                stream.push_str(&document[split..]);
                if let Some(preview) = stream.preview_final() {
                    latest = preview;
                }
                assert_eq!(latest, json!({"a": expected}), "split {split}");
            }
        }
    }

    #[test]
    fn documents_deliberate_incomplete_preview_differences() {
        assert_eq!(exact_after_chunks(r#"{"a":nul"#, 1), json!({"a": null}));
        assert_eq!(exact_after_chunks(r#"{"a":tru"#, 1), json!({"a": true}));
        assert_eq!(exact_after_chunks(r#"{"a":2.5e+"#, 1), json!({"a": 2.5}));
        assert_eq!(exact_after_chunks("{\"a\":\"x\\", 1), json!({"a": "x"}));
        assert_eq!(exact_after_chunks("{\"a\":\"x\\u12", 1), json!({"a": "x"}));
        assert_eq!(exact_after_chunks("{\"a\":\"x  ", 1), json!({"a": "x  "}));
        assert_eq!(
            exact_after_chunks(r#"{"a" 1 "b":2}"#, 1),
            json!({"a": 1, "b": 2})
        );
    }

    #[test]
    fn snapshots_are_owned_and_do_not_change() {
        let mut stream = ToolCallArgumentsStream::new();
        let first = stream
            .append(r#"{"a":"x"#)
            .expect("small first snapshot is affordable");
        let saved = first.clone();
        let _ = stream.append(r#"","b":2}"#);
        assert_eq!(first, saved);
        assert_eq!(saved, json!({"a": "x"}));
    }

    #[test]
    fn replace_clear_and_take_reset_parser_state() {
        let mut stream = ToolCallArgumentsStream::new();
        stream.push_str(r#"{"a":"#);
        let replaced = stream.replace(r#"{"a":1}"#);
        assert_eq!(replaced.delta.as_deref(), Some("1}"));
        assert_eq!(replaced.preview, Some(json!({"a": 1})));
        assert_eq!(stream.replace(r#"{"a":1}"#).delta, None);

        let reset = stream.replace(r#"{"b":[2]}"#);
        assert_eq!(reset.delta, None);
        assert_eq!(reset.preview, Some(json!({"b": [2]})));

        assert_eq!(stream.take_text(), r#"{"b":[2]}"#);
        assert!(stream.is_empty());
        stream.push_str(r#"{"c":3}"#);
        assert_eq!(stream.preview_final(), Some(json!({"c": 3})));
        stream.clear();
        stream.push_str(r#"[4]"#);
        assert_eq!(stream.preview_final(), Some(json!({})));
    }

    #[test]
    fn empty_replacement_clears_previous_preview() {
        let mut stream = ToolCallArgumentsStream::new();
        stream.push_str(r#"{"a":1}"#);
        assert_eq!(stream.preview_final(), Some(json!({"a": 1})));
        for replacement in ["", "  \n\t"] {
            let replaced = stream.replace(replacement);
            assert_eq!(replaced.delta, None);
            assert_eq!(replaced.preview, Some(json!({})));
            assert_eq!(stream.text(), replacement);
            stream.replace(r#"{"a":1}"#);
        }
    }

    #[test]
    fn final_validation_never_uses_repaired_preview() {
        let raw = r#"{"command":"rm -rf /","unexpected""#;
        let mut stream = ToolCallArgumentsStream::new();
        stream.push_str(raw);
        assert_eq!(stream.preview_final(), Some(json!({"command": "rm -rf /"})));
        let (arguments, error) = stream.finish();
        assert_eq!(arguments, json!({}));
        assert!(error.is_some_and(|message| message.contains(raw)));
        assert_eq!(stream.text(), raw);

        stream.clear();
        stream.push_str("[1,2]");
        assert!(stream.finish().1.is_some());
    }

    #[test]
    fn malformed_inputs_do_not_panic() {
        for input in [
            "",
            "{{{{",
            "]}}} garbage",
            r#"{"a": truX}"#,
            "[1,2,",
            "\\",
            "\0\n\t",
            r#"{"__proto__":"x"}"#,
            r#"{"a":"\u12zz"}"#,
        ] {
            let mut stream = ToolCallArgumentsStream::new();
            for chunk in chunks_at_boundaries(input, 1) {
                let _ = stream.append(chunk);
            }
            let _ = stream.preview_final();
            let _ = stream.finish();
        }
    }

    #[test]
    fn excessive_depth_does_not_build_an_unbounded_value_tree() {
        let depth = 2_000;
        let raw = format!("{}0{}", r#"{"a":"#.repeat(depth), "}".repeat(depth));
        let mut stream = ToolCallArgumentsStream::new();
        for chunk in chunks_at_boundaries(&raw, 17) {
            let _ = stream.append(chunk);
        }
        let preview = stream
            .preview_final()
            .expect("bounded object preview should exist");
        assert!(preview.is_object());
        assert_eq!(stream.text(), raw);
        assert!(stream.finish().1.is_some());
    }

    #[test]
    fn large_open_string_uses_budget_and_final_catches_up() {
        let raw = format!(r#"{{"content":"{}"}}"#, "abc123".repeat(16 * 1024));
        let mut stream = ToolCallArgumentsStream::new();
        let mut delayed = false;
        for chunk in chunks_at_boundaries(&raw, 64) {
            if stream.append(chunk).is_none() {
                delayed = true;
            }
        }
        assert!(delayed, "large repeated deep copies should hit the budget");
        let preview = stream.preview_final().expect("final preview must be fresh");
        assert_eq!(
            preview["content"]
                .as_str()
                .expect("content must remain a string")
                .len(),
            96 * 1024
        );
        assert_eq!(stream.finish(), (preview, None));
    }
}
