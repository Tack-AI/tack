//! OpenRPC → Rust codegen for tack-RPC v3.
//!
//! Supported schema subset (deliberately small — the schema is written
//! against it): `object` + `properties`/`required` → struct, `string` +
//! `enum` → enum, `array` → `Vec`, `object` + typed `additionalProperties`
//! → `BTreeMap`, primitives with integer `format` hints, `$ref`,
//! `oneOf: [X, null]` / `type: [T, "null"]` → `Option`, and anything else
//! (free-form JSON) → `serde_json::Value`.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

const SCHEMA_PATH: &str = "protocol/tack-rpc.openrpc.json";
const OUT_PATH: &str = "crates/tack-ext/src/rpc3.rs";

/// Regenerate (or with `check`, verify) the Rust types from the schema.
pub fn codegen(check: bool) -> Result<()> {
    let root = workspace_root();
    let raw = std::fs::read_to_string(root.join(SCHEMA_PATH))
        .with_context(|| format!("read {SCHEMA_PATH}"))?;
    let doc: Value = serde_json::from_str(&raw).context("parse OpenRPC document")?;
    let rendered = rustfmt(&generate(&doc)?)?;
    if check {
        let current = std::fs::read_to_string(root.join(OUT_PATH)).unwrap_or_default();
        if current != rendered {
            bail!("{OUT_PATH} is stale — run `cargo run -p xtask -- codegen`");
        }
        println!("{OUT_PATH} is up to date");
    } else {
        std::fs::write(root.join(OUT_PATH), &rendered)
            .with_context(|| format!("write {OUT_PATH}"))?;
        println!("wrote {OUT_PATH}");
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace root")
        .to_path_buf()
}

/// Format through rustfmt (stdin/stdout) so the committed file is
/// `cargo fmt --check` clean by construction.
fn rustfmt(source: &str) -> Result<String> {
    let mut child = std::process::Command::new("rustfmt")
        .args(["--edition", "2024"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn rustfmt")?;
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(source.as_bytes())
        .context("feed rustfmt")?;
    let output = child.wait_with_output().context("wait for rustfmt")?;
    if !output.status.success() {
        bail!(
            "rustfmt failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).context("rustfmt output is not utf-8")
}

fn generate(doc: &Value) -> Result<String> {
    let mut out = String::new();
    out.push_str(
        "//! tack-RPC v3 types — GENERATED from `protocol/tack-rpc.openrpc.json`\n\
         //! by `cargo run -p xtask -- codegen`. Do not edit by hand.\n\
         #![allow(clippy::doc_markdown)]\n\n\
         use std::collections::BTreeMap;\n\n\
         use serde::{Deserialize, Serialize};\n\
         use serde_json::Value;\n\n",
    );
    out.push_str(PREAMBLE);
    emit_method_constants(doc, &mut out)?;
    let schemas = doc
        .pointer("/components/schemas")
        .and_then(Value::as_object)
        .context("components.schemas missing")?;
    for (name, schema) in schemas {
        emit_named_type(name, schema, &mut out)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Methods → constants
// ---------------------------------------------------------------------------

fn emit_method_constants(doc: &Value, out: &mut String) -> Result<()> {
    let methods = doc
        .get("methods")
        .and_then(Value::as_array)
        .context("methods missing")?;
    out.push_str(
        "\n/// Wire method names (see the OpenRPC document for contracts).\npub mod method {\n",
    );
    for method in methods {
        let name = method
            .get("name")
            .and_then(Value::as_str)
            .context("method without name")?;
        let mut doc_line = String::new();
        if let Some(direction) = method.get("x-direction").and_then(Value::as_str) {
            let _ = write!(doc_line, "[{direction}] ");
        }
        if let Some(description) = method.get("description").and_then(Value::as_str) {
            doc_line.push_str(description);
        }
        emit_doc(out, &doc_line, 1);
        let const_name = name
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty())
            .map(field_ident)
            .collect::<Vec<_>>()
            .join("_")
            .to_uppercase();
        let _ = writeln!(out, "    pub const {const_name}: &str = \"{name}\";\n");
    }
    out.push_str("}\n");
    Ok(())
}

// ---------------------------------------------------------------------------
// Named component schemas → types
// ---------------------------------------------------------------------------

fn emit_named_type(name: &str, schema: &Value, out: &mut String) -> Result<()> {
    out.push('\n');
    if let Some(description) = schema.get("description").and_then(Value::as_str) {
        emit_doc(out, description, 0);
    }
    if let Some(variants) = string_enum_variants(schema) {
        out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]\n");
        let _ = writeln!(out, "pub enum {name} {{");
        for variant in &variants {
            let _ = writeln!(out, "    #[serde(rename = \"{variant}\")]");
            let _ = writeln!(out, "    {},", pascal_case(variant));
        }
        out.push_str("}\n");
        return Ok(());
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        out.push_str("#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]\n");
        let _ = writeln!(out, "pub struct {name} {{");
        for (wire_name, property) in properties {
            emit_field(
                out,
                wire_name,
                property,
                required.contains(&wire_name.as_str()),
            )?;
        }
        out.push_str("}\n");
        return Ok(());
    }
    // Map or alias of another shape.
    let _ = writeln!(out, "pub type {name} = {};", rust_type(schema)?);
    Ok(())
}

fn emit_field(out: &mut String, wire_name: &str, property: &Value, required: bool) -> Result<()> {
    if let Some(description) = property.get("description").and_then(Value::as_str) {
        emit_doc(out, description, 1);
    }
    let ty = rust_type(property)?;
    let ty = if required || ty.starts_with("Option<") {
        ty
    } else {
        format!("Option<{ty}>")
    };
    let _ = writeln!(out, "    #[serde(rename = \"{wire_name}\")]");
    if !required {
        out.push_str("    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n");
    }
    let _ = writeln!(out, "    pub {}: {ty},", field_ident(wire_name));
    Ok(())
}

/// The Rust type for an inline schema.
fn rust_type(schema: &Value) -> Result<String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return Ok(ref_name(reference)?.to_string());
    }
    if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
        let mut non_null: Vec<&Value> = one_of
            .iter()
            .filter(|variant| variant.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        if non_null.len() == 1 && non_null.len() < one_of.len() {
            let inner = rust_type(non_null.remove(0))?;
            return Ok(format!("Option<{inner}>"));
        }
        bail!("unsupported oneOf shape (only [T, null] is supported): {one_of:?}");
    }
    let ty = schema.get("type");
    // `type: [T, "null"]` → Option<T>.
    if let Some(types) = ty.and_then(Value::as_array) {
        let mut non_null: Vec<&Value> = types
            .iter()
            .filter(|t| t.as_str() != Some("null"))
            .collect();
        if non_null.len() == 1 && non_null.len() < types.len() {
            let mut clone = schema.clone();
            if let Some(object) = clone.as_object_mut() {
                object.insert("type".to_string(), non_null.remove(0).clone());
            }
            return Ok(format!("Option<{}>", rust_type(&clone)?));
        }
        bail!("unsupported type union: {types:?}");
    }
    Ok(match ty.and_then(Value::as_str) {
        Some("string") => "String".to_string(),
        Some("integer") => match schema.get("format").and_then(Value::as_str) {
            Some("uint64") => "u64".to_string(),
            Some("uint32") => "u32".to_string(),
            Some("int32") => "i32".to_string(),
            _ => "i64".to_string(),
        },
        Some("number") => "f64".to_string(),
        Some("boolean") => "bool".to_string(),
        Some("array") => format!("Vec<{}>", rust_type(items_schema(schema)?)?),
        Some("object") => match schema.get("additionalProperties") {
            Some(additional) if additional.is_object() => {
                format!("BTreeMap<String, {}>", rust_type(additional)?)
            }
            _ => "Value".to_string(),
        },
        // Free-form JSON (no type, no constraints).
        _ => "Value".to_string(),
    })
}

fn items_schema(schema: &Value) -> Result<&Value> {
    schema.get("items").context("array schema without items")
}

fn ref_name(reference: &str) -> Result<&str> {
    reference
        .strip_prefix("#/components/schemas/")
        .with_context(|| format!("unsupported $ref {reference:?}"))
}

fn string_enum_variants(schema: &Value) -> Option<Vec<String>> {
    if schema.get("type").and_then(Value::as_str) != Some("string") {
        return None;
    }
    let variants: Vec<String> = schema
        .get("enum")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if variants.is_empty() {
        None
    } else {
        Some(variants)
    }
}

// ---------------------------------------------------------------------------
// Identifiers and doc comments
// ---------------------------------------------------------------------------

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "box", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "final", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "macro",
    "match", "mod", "move", "mut", "override", "priv", "pub", "ref", "return", "self", "static",
    "struct", "super", "trait", "true", "try", "type", "typeof", "unsafe", "unsized", "use",
    "virtual", "where", "while", "yield",
];

/// Wire field name (camelCase) → snake_case Rust ident (raw if keyword).
fn field_ident(wire_name: &str) -> String {
    let mut out = String::new();
    let mut prev_lower_or_digit = false;
    for c in wire_name.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower_or_digit {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower_or_digit = false;
        } else {
            out.push(c);
            prev_lower_or_digit = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    if KEYWORDS.contains(&out.as_str()) {
        format!("r#{out}")
    } else {
        out
    }
}

/// Enum wire value (camelCase / kebab) → PascalCase variant name.
fn pascal_case(value: &str) -> String {
    let mut out = String::new();
    let mut capitalize = true;
    for c in value.chars() {
        if !c.is_ascii_alphanumeric() {
            capitalize = true;
            continue;
        }
        if capitalize {
            out.push(c.to_ascii_uppercase());
            capitalize = false;
        } else {
            out.push(c);
        }
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, 'N');
    }
    out
}

/// Emit a `///` doc comment at the given indent (in spaces), sanitized so
/// generated docs survive `RUSTDOCFLAGS="-D warnings"` (no intra-doc link
/// false positives, no comment terminators).
fn emit_doc(out: &mut String, text: &str, indent: usize) {
    let pad = " ".repeat(indent * 4);
    for line in text.lines() {
        let line = line
            .replace("*/", "* /")
            .replace('[', "\\[")
            .replace(']', "\\]")
            .replace('<', "\\<")
            .replace('>', "\\>");
        if line.is_empty() {
            let _ = writeln!(out, "{pad}///");
        } else {
            let _ = writeln!(out, "{pad}/// {line}");
        }
    }
}

// ---------------------------------------------------------------------------
// Static preamble: JSON-RPC 2.0 envelopes and error codes
// ---------------------------------------------------------------------------

const PREAMBLE: &str = r#"/// The only legal `jsonrpc` field value.
pub const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC 2.0 error codes.
pub const ERR_PARSE: i64 = -32700;
pub const ERR_INVALID_REQUEST: i64 = -32600;
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_INVALID_PARAMS: i64 = -32602;
pub const ERR_INTERNAL: i64 = -32603;

// tack-RPC domain error codes (components.errors in the schema).
/// Denied by host policy (for example exec in an untrusted context).
pub const ERR_POLICY_DENIED: i64 = -32001;
/// The plugin did not declare / was not granted this capability.
pub const ERR_CAPABILITY_NOT_GRANTED: i64 = -32002;
/// The plugin carrier is dead or unreachable.
pub const ERR_PLUGIN_UNAVAILABLE: i64 = -32003;
/// The request exceeded the host's call timeout.
pub const ERR_REQUEST_TIMEOUT: i64 = -32004;

/// A JSON-RPC request/response id (per-sender numbering).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// Numeric id (the common case).
    Num(u64),
    /// String id (tolerated for JSON-RPC compliance).
    Str(String),
}

impl From<u64> for Id {
    fn from(value: u64) -> Self {
        Id::Num(value)
    }
}

/// A JSON-RPC 2.0 request (either direction; both peers may issue).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Id,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// Build a request with a typed params payload.
    pub fn new(id: impl Into<Id>, method: impl Into<String>, params: Value) -> Self {
        Request {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params: Some(params),
        }
    }
}

/// A JSON-RPC 2.0 notification (no id, no response).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    /// Build a notification with a typed params payload.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Notification {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params: Some(params),
        }
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A JSON-RPC 2.0 response. `id` is null when the request could not be
/// parsed; exactly one of `result`/`error` is present in a valid response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    /// A success response.
    pub fn result(id: Option<Id>, result: Value) -> Self {
        Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error response.
    pub fn error(id: Option<Id>, code: i64, message: impl Into<String>) -> Self {
        Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(ErrorObject {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}
"#;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn field_ident_snake_cases_and_protects_keywords() {
        assert_eq!(field_ident("toolCallId"), "tool_call_id");
        assert_eq!(field_ident("cursorOffset"), "cursor_offset");
        assert_eq!(field_ident("type"), "r#type");
        assert_eq!(field_ident("message"), "message");
    }

    #[test]
    fn pascal_case_variants() {
        assert_eq!(pascal_case("statusLineSegment"), "StatusLineSegment");
        assert_eq!(pascal_case("askUser"), "AskUser");
        assert_eq!(pascal_case("not-available"), "NotAvailable");
    }

    #[test]
    fn rust_type_shapes() {
        assert_eq!(
            rust_type(&serde_json::json!({"type": "integer", "format": "uint64"})).unwrap(),
            "u64"
        );
        assert_eq!(
            rust_type(&serde_json::json!({"type": "array", "items": {"type": "string"}})).unwrap(),
            "Vec<String>"
        );
        assert_eq!(
            rust_type(&serde_json::json!({
                "type": "object",
                "additionalProperties": {"$ref": "#/components/schemas/MetricOperation"}
            }))
            .unwrap(),
            "BTreeMap<String, MetricOperation>"
        );
        assert_eq!(
            rust_type(&serde_json::json!({
                "oneOf": [{"$ref": "#/components/schemas/Verdict"}, {"type": "null"}]
            }))
            .unwrap(),
            "Option<Verdict>"
        );
        assert_eq!(
            rust_type(&serde_json::json!({"type": ["string", "null"]})).unwrap(),
            "Option<String>"
        );
        assert_eq!(rust_type(&serde_json::json!({})).unwrap(), "Value");
    }

    /// The schema must stay within the supported subset (this is also the
    /// guard that keeps `cargo run -p xtask -- codegen` total).
    #[test]
    fn real_schema_generates() {
        let root = workspace_root();
        let raw = std::fs::read_to_string(root.join(SCHEMA_PATH)).unwrap();
        let doc: Value = serde_json::from_str(&raw).unwrap();
        let out = generate(&doc).unwrap();
        assert!(out.contains("pub struct InitializeParams"));
        assert!(out.contains("pub enum RunMode"));
        assert!(out.contains("pub const TOOLS_EXECUTE"));
    }
}
