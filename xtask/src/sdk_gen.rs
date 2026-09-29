//! TypeScript (.d.ts) and Python (TypedDict/Enum) emission from the
//! OpenRPC document, keeping every SDK on the same single source.

use std::fmt::Write as _;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::codegen::{
    component_schemas, description_of, field_ident, method_const_name, methods,
    string_enum_variants,
};

const HEADER: &str = "GENERATED from protocol/tack-rpc.openrpc.json by `cargo run -p xtask -- codegen`. Do not edit by hand.";

/// JS numbers are IEEE-754 doubles: integer-valued schema fields
/// (uint64 in Rust) lose precision above 2^53. Noted on the generated
/// TypeScript header so SDK consumers are not surprised.
const TS_UINT64_NOTE: &str = "Note: uint64 schema fields (cursorOffset, messageCount, token usage, timeoutMs) are plain JS `number` here — values above 2^53 lose precision.";

/// Error codes + method constants shared by both emitters.
fn error_codes() -> &'static [(&'static str, i64)] {
    &[
        ("ERR_PARSE", -32700),
        ("ERR_INVALID_REQUEST", -32600),
        ("ERR_METHOD_NOT_FOUND", -32601),
        ("ERR_INVALID_PARAMS", -32602),
        ("ERR_INTERNAL", -32603),
        ("ERR_POLICY_DENIED", -32001),
        ("ERR_CAPABILITY_NOT_GRANTED", -32002),
        ("ERR_PLUGIN_UNAVAILABLE", -32003),
        ("ERR_REQUEST_TIMEOUT", -32004),
    ]
}

// ---------------------------------------------------------------------------
// TypeScript
// ---------------------------------------------------------------------------

pub fn generate_ts(doc: &Value) -> Result<String> {
    let mut out = String::new();
    let _ = writeln!(out, "// {HEADER}");
    let _ = writeln!(out, "// {TS_UINT64_NOTE}\n");
    for (name, code) in error_codes() {
        let _ = writeln!(out, "export const {name} = {code};");
    }
    out.push('\n');
    for method in methods(doc)? {
        let name = method
            .get("name")
            .and_then(Value::as_str)
            .context("method without name")?;
        let description = ts_doc_line(method);
        if !description.is_empty() {
            let _ = writeln!(out, "/** {description} */");
        }
        let _ = writeln!(
            out,
            "export const {} = \"{name}\";",
            method_const_name(name)
        );
    }
    for (name, schema) in component_schemas(doc)? {
        out.push('\n');
        ts_named_type(&mut out, name, schema)?;
    }
    Ok(out)
}

fn ts_doc_line(value: &Value) -> String {
    let direction = value
        .get("x-direction")
        .and_then(Value::as_str)
        .unwrap_or("");
    let description = description_of(value);
    let line = if direction.is_empty() {
        description.to_string()
    } else {
        format!("[{direction}] {description}")
    };
    line.replace("*/", "* /")
}

fn ts_named_type(out: &mut String, name: &str, schema: &Value) -> Result<()> {
    let description = description_of(schema).replace("*/", "* /");
    if !description.is_empty() {
        let _ = writeln!(out, "/** {description} */");
    }
    if let Some(variants) = string_enum_variants(schema) {
        let union = variants
            .iter()
            .map(|v| format!("\"{v}\""))
            .collect::<Vec<_>>()
            .join(" | ");
        let _ = writeln!(out, "export type {name} = {union};\n");
        return Ok(());
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let _ = writeln!(out, "export interface {name} {{");
        for (wire_name, property) in properties {
            let description = description_of(property).replace("*/", "* /");
            if !description.is_empty() {
                let _ = writeln!(out, "  /** {description} */");
            }
            let optional = if required.contains(&wire_name.as_str()) {
                ""
            } else {
                "?"
            };
            let _ = writeln!(out, "  \"{wire_name}\"{optional}: {};", ts_type(property)?);
        }
        out.push_str("}\n");
        return Ok(());
    }
    let _ = writeln!(out, "export type {name} = {};", ts_type(schema)?);
    Ok(())
}

fn ts_type(schema: &Value) -> Result<String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return Ok(ref_name(reference)?.to_string());
    }
    if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
        let mut non_null: Vec<&Value> = one_of
            .iter()
            .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        if non_null.len() == 1 && non_null.len() < one_of.len() {
            return Ok(format!("{} | null", ts_type(non_null.remove(0))?));
        }
        bail!("unsupported oneOf shape: {one_of:?}");
    }
    let ty = schema.get("type");
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
            return Ok(format!("{} | null", ts_type(&clone)?));
        }
        bail!("unsupported type union: {types:?}");
    }
    Ok(match ty.and_then(Value::as_str) {
        Some("string") => "string".to_string(),
        Some("integer") | Some("number") => "number".to_string(),
        Some("boolean") => "boolean".to_string(),
        Some("array") => format!(
            "Array<{}>",
            ts_type(schema.get("items").context("array without items")?)?
        ),
        Some("object") => match schema.get("additionalProperties") {
            Some(additional) if additional.is_object() => {
                format!("Record<string, {}>", ts_type(additional)?)
            }
            _ => "any".to_string(),
        },
        _ => "any".to_string(),
    })
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

/// Which typing/enum imports the emitted Python body actually uses —
/// the header is rendered from this so unused imports (and duplicate
/// import lines) never appear.
#[derive(Default)]
struct PyUsage {
    any: bool,
    optional: bool,
    not_required: bool,
    typed_dict: bool,
    enumeration: bool,
}

pub fn generate_py(doc: &Value) -> Result<String> {
    let mut body = String::new();
    let mut usage = PyUsage::default();
    for (name, code) in error_codes() {
        let _ = writeln!(body, "{name} = {code}");
    }
    body.push('\n');
    for method in methods(doc)? {
        let name = method
            .get("name")
            .and_then(Value::as_str)
            .context("method without name")?;
        let _ = writeln!(body, "{} = \"{name}\"", method_const_name(name));
    }
    for (name, schema) in component_schemas(doc)? {
        body.push('\n');
        py_named_type(&mut body, name, schema, &mut usage)?;
    }

    let mut out = String::new();
    let _ = writeln!(out, "# {HEADER}\n");
    if usage.enumeration {
        out.push_str("from enum import Enum\n");
    }
    let mut typing: Vec<&str> = Vec::new();
    if usage.any {
        typing.push("Any");
    }
    if usage.not_required {
        typing.push("NotRequired");
    }
    if usage.optional {
        typing.push("Optional");
    }
    if usage.typed_dict {
        typing.push("TypedDict");
    }
    if !typing.is_empty() {
        let _ = writeln!(out, "from typing import {}", typing.join(", "));
    }
    out.push('\n');
    out.push_str(&body);
    Ok(out)
}

fn py_named_type(out: &mut String, name: &str, schema: &Value, usage: &mut PyUsage) -> Result<()> {
    if let Some(variants) = string_enum_variants(schema) {
        usage.enumeration = true;
        let _ = writeln!(out, "class {name}(str, Enum):");
        let description = description_of(schema);
        if !description.is_empty() {
            let _ = writeln!(
                out,
                "    \"\"\"{}\"\"\"",
                description.replace("\"\"\"", "\"\" \"")
            );
        }
        for variant in &variants {
            let member = field_ident(variant).to_uppercase();
            let _ = writeln!(out, "    {member} = \"{variant}\"");
        }
        out.push('\n');
        return Ok(());
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        usage.typed_dict = true;
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let _ = writeln!(out, "class {name}(TypedDict):");
        let description = description_of(schema);
        if !description.is_empty() {
            let _ = writeln!(
                out,
                "    \"\"\"{}\"\"\"",
                description.replace("\"\"\"", "\"\" \"")
            );
        }
        if properties.is_empty() {
            out.push_str("    pass\n");
        }
        for (wire_name, property) in properties {
            let ty = py_type(property, usage)?;
            if required.contains(&wire_name.as_str()) {
                let _ = writeln!(out, "    {wire_name}: {ty}");
            } else {
                usage.not_required = true;
                let _ = writeln!(out, "    {wire_name}: NotRequired[{ty}]");
            }
        }
        out.push('\n');
        return Ok(());
    }
    let _ = writeln!(out, "{name} = {}\n", py_type(schema, usage)?);
    Ok(())
}

fn py_type(schema: &Value, usage: &mut PyUsage) -> Result<String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return Ok(format!("\"{}\"", ref_name(reference)?));
    }
    if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
        let mut non_null: Vec<&Value> = one_of
            .iter()
            .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        if non_null.len() == 1 && non_null.len() < one_of.len() {
            usage.optional = true;
            return Ok(format!("Optional[{}]", py_type(non_null.remove(0), usage)?));
        }
        bail!("unsupported oneOf shape: {one_of:?}");
    }
    let ty = schema.get("type");
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
            usage.optional = true;
            return Ok(format!("Optional[{}]", py_type(&clone, usage)?));
        }
        bail!("unsupported type union: {types:?}");
    }
    Ok(match ty.and_then(Value::as_str) {
        Some("string") => "str".to_string(),
        Some("integer") => "int".to_string(),
        Some("number") => "float".to_string(),
        Some("boolean") => "bool".to_string(),
        Some("array") => format!(
            "list[{}]",
            py_type(schema.get("items").context("array without items")?, usage)?
        ),
        Some("object") => match schema.get("additionalProperties") {
            Some(additional) if additional.is_object() => {
                format!("dict[str, {}]", py_type(additional, usage)?)
            }
            _ => {
                usage.any = true;
                "Any".to_string()
            }
        },
        _ => {
            usage.any = true;
            "Any".to_string()
        }
    })
}

fn ref_name(reference: &str) -> Result<&str> {
    reference
        .strip_prefix("#/components/schemas/")
        .with_context(|| format!("unsupported $ref {reference:?}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::codegen::{SCHEMA_PATH, workspace_root};

    /// The schema must stay within the subset both emitters support.
    #[test]
    fn real_schema_generates_ts_and_py() {
        let raw = std::fs::read_to_string(workspace_root().join(SCHEMA_PATH)).unwrap();
        let doc: Value = serde_json::from_str(&raw).unwrap();
        let ts = generate_ts(&doc).unwrap();
        assert!(ts.contains("export interface InitializeParams"));
        assert!(ts.contains("export type RunMode = \"tui\" | \"print\" | \"rpc\" | \"acp\";"));
        assert!(ts.contains("export const TOOLS_EXECUTE = \"tools/execute\";"));
        let py = generate_py(&doc).unwrap();
        assert!(py.contains("class InitializeParams(TypedDict):"));
        assert!(py.contains("class RunMode(str, Enum):"));
        assert!(py.contains("TUI = \"tui\""));
        // Imports are emitted once and only when the schema uses them.
        assert_eq!(py.matches("from typing import").count(), 1);
        assert!(py.contains("from typing import Any, NotRequired, TypedDict"));
        assert!(!py.contains("Optional"), "unused Optional import: {py}");
        assert!(ts.contains("values above 2^53 lose precision"));
    }
}
