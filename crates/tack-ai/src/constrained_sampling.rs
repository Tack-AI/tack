//! Constrained sampling (port of `api/constrained-sampling.ts`): strict JSON
//! schema subset conversion for OpenAI strict tools, plus grammar-constrained
//! tools (Lark/regex) declaration, input-property inference, and the
//! input_json delta re-packager for streaming.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `Tool.constrainedSampling` declaration (TS types.ts).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSampling {
    /// Strict JSON-schema tool. `strict`: "require" | "auto" (default auto).
    JsonSchema {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<String>,
    },
    /// Grammar-constrained tool with provider-specific grammar variants.
    Grammar { variants: GrammarVariants },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GrammarVariants {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_lark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_regex: Option<String>,
}

const UNSUPPORTED_STRICT_SCHEMA_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

fn is_structured_schema(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    let types: Vec<&str> = match obj.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    types.contains(&"object")
        || types.contains(&"array")
        || obj.contains_key("properties")
        || obj.contains_key("items")
}

fn schema_allows_null(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    if obj.get("type") == Some(&Value::String("null".to_string())) {
        return true;
    }
    if let Some(Value::Array(types)) = obj.get("type")
        && types.iter().any(|t| t == "null")
    {
        return true;
    }
    if obj.get("const") == Some(&Value::Null) {
        return true;
    }
    if let Some(Value::Array(variants)) = obj.get("enum")
        && variants.contains(&Value::Null)
    {
        return true;
    }
    matches!(obj.get("anyOf"), Some(Value::Array(variants)) if variants.iter().any(schema_allows_null)
    )
}

fn make_node_strict(schema: &mut Value) -> Result<(), String> {
    let Some(obj) = schema.as_object_mut() else {
        return Err("boolean schemas are unsupported".to_string());
    };
    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if obj.contains_key(*key) {
            return Err(format!("{key} schemas are unsupported"));
        }
    }

    if let Some(any_of) = obj.get_mut("anyOf") {
        let Some(variants) = any_of.as_array_mut() else {
            return Err("anyOf must contain at least one schema".to_string());
        };
        if variants.is_empty() {
            return Err("anyOf must contain at least one schema".to_string());
        }
        for variant in variants.iter_mut() {
            if is_structured_schema(variant) {
                return Err("object and array unions are unsupported".to_string());
            }
            make_node_strict(variant)?;
        }
    }

    if let Some(items) = obj.get_mut("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".to_string());
        }
        make_node_strict(items)?;
    }

    let is_object = obj.get("type") == Some(&Value::String("object".to_string()));
    if obj.contains_key("properties") && !is_object {
        return Err("properties require type object".to_string());
    }
    if !is_object {
        return Ok(());
    }
    if let Some(ap) = obj.get("additionalProperties")
        && *ap != Value::Bool(false)
    {
        return Err("schema-valued or true additionalProperties is unsupported".to_string());
    }
    if let Some(properties) = obj.get("properties")
        && !properties.is_object()
    {
        return Err("object properties must be a schema map".to_string());
    }
    if let Some(required) = obj.get("required")
        && (!required.is_array()
            || required
                .as_array()
                .is_some_and(|r| r.iter().any(|k| !k.is_string())))
    {
        return Err("object required must be a string array".to_string());
    }

    let mut properties = obj
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let property_names: Vec<String> = properties.keys().cloned().collect();
    let required: std::collections::HashSet<String> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|r| {
            r.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if required.iter().any(|key| !property_names.contains(key)) {
        return Err("required contains an unknown property".to_string());
    }
    for (key, property) in properties.iter_mut() {
        make_node_strict(property)?;
        if !required.contains(key) && !schema_allows_null(property) {
            let original = property.clone();
            *property = Value::Array(vec![
                original,
                Value::Object(Map::from_iter([(
                    "type".to_string(),
                    Value::String("null".to_string()),
                )])),
            ]);
            // Wrap as anyOf.
            let any_of = Value::Object(Map::from_iter([("anyOf".to_string(), property.clone())]));
            *property = any_of;
        }
    }
    obj.insert("properties".to_string(), Value::Object(properties));
    obj.insert(
        "required".to_string(),
        Value::Array(property_names.into_iter().map(Value::String).collect()),
    );
    obj.insert("additionalProperties".to_string(), Value::Bool(false));
    Ok(())
}

/// Convert a tool schema to the strict subset expected by provider
/// constrained sampling (TS makeStrictJsonSchema).
pub fn make_strict_json_schema(schema: &Value) -> Result<Value, String> {
    if !schema.is_object() {
        return Err("root schema must have type object".to_string());
    }
    let mut cloned = schema.clone();
    make_node_strict(&mut cloned)?;
    if cloned.get("type") != Some(&Value::String("object".to_string())) {
        return Err("root schema must have type object".to_string());
    }
    Ok(cloned)
}

/// TS resolveJsonSchemaStrictSampling: Some(true) = emit strict, None = no
/// constraint, Err = "require" violated.
pub fn resolve_json_schema_strict(
    tool: &crate::types::ToolDefinition,
    supports_strict_mode: bool,
) -> Result<Option<bool>, String> {
    let Some(ConstrainedSampling::JsonSchema { strict }) = &tool.constrained_sampling else {
        return Ok(None);
    };
    let require = strict.as_deref() == Some("require");
    if supports_strict_mode {
        match make_strict_json_schema(&tool.parameters) {
            Ok(_) => return Ok(Some(true)),
            Err(e) => {
                if require {
                    return Err(format!(
                        "Tool \"{}\" requires JSON-schema constrained sampling, but {e}.",
                        tool.name
                    ));
                }
                return Ok(None);
            }
        }
    }
    if require {
        return Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ));
    }
    Ok(None)
}

/// Resolved grammar constraint for one tool.
#[derive(Clone, Debug)]
pub struct GrammarConstraint {
    pub format: String, // "lark" | "regex"
    pub definition: String,
    pub input_property: String,
}

fn infer_grammar_input_property(tool: &crate::types::ToolDefinition) -> Result<String, String> {
    let schema = &tool.parameters;
    if schema.get("type") != Some(&Value::String("object".to_string())) {
        return Err("grammar constrained sampling requires an object parameter schema".to_string());
    }
    let required = schema.get("required").and_then(Value::as_array);
    let Some(required) = required else {
        return Err(
            "grammar constrained sampling requires exactly one required string property"
                .to_string(),
        );
    };
    if required.len() != 1 || !required[0].is_string() {
        return Err(
            "grammar constrained sampling requires exactly one required string property"
                .to_string(),
        );
    }
    let input_property = required[0].as_str().expect("checked").to_string();
    let property = schema
        .get("properties")
        .and_then(|p| p.get(&input_property));
    let Some(property) = property else {
        return Err(format!(
            "grammar constrained sampling requires a properties entry for {input_property}"
        ));
    };
    if property.get("type") != Some(&Value::String("string".to_string())) {
        return Err(format!(
            "grammar constrained sampling property {input_property} must have type string"
        ));
    }
    Ok(input_property)
}

/// TS resolveGrammarConstrainedSampling.
pub fn resolve_grammar(
    tool: &crate::types::ToolDefinition,
    supports_openai_grammar_tools: bool,
) -> Result<Option<GrammarConstraint>, String> {
    let Some(ConstrainedSampling::Grammar { variants }) = &tool.constrained_sampling else {
        return Ok(None);
    };
    if !supports_openai_grammar_tools {
        return Ok(None);
    }
    let lark = variants
        .openai_lark
        .as_ref()
        .filter(|d| !d.trim().is_empty())
        .cloned();
    let regex = variants
        .openai_regex
        .as_ref()
        .filter(|d| !d.trim().is_empty())
        .cloned();
    let (format, definition) = match (lark, regex) {
        (Some(l), _) => ("lark", l),
        (None, Some(r)) => ("regex", r),
        (None, None) => {
            return Err(format!(
                "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
                tool.name
            ));
        }
    };
    let input_property = infer_grammar_input_property(tool).map_err(|e| {
        format!(
            "Tool \"{}\" cannot use grammar constrained sampling: {e}.",
            tool.name
        )
    })?;
    Ok(Some(GrammarConstraint {
        format: format.to_string(),
        definition,
        input_property,
    }))
}

/// tool name → input property, for every grammar tool in the set.
pub fn create_grammar_tool_input_properties(
    tools: &[crate::types::ToolDefinition],
    supports_openai_grammar_tools: bool,
) -> std::collections::HashMap<String, String> {
    let mut properties = std::collections::HashMap::new();
    for tool in tools {
        if let Ok(Some(grammar)) = resolve_grammar(tool, supports_openai_grammar_tools) {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    properties
}

/// TS getGrammarToolInput: extract the grammar input string from tool-call
/// arguments (assistant messages on the wire send plain grammar text).
pub fn get_grammar_tool_input(
    tool_name: &str,
    arguments: &Value,
    input_property: &str,
) -> Result<String, String> {
    match arguments.get(input_property).and_then(Value::as_str) {
        Some(input) => Ok(input.to_string()),
        None => Err(format!(
            "Grammar tool call \"{tool_name}\" requires argument \"{input_property}\" to be a string."
        )),
    }
}

/// Streaming input_json delta re-packager (TS appendGrammarToolInputJsonDelta):
/// the model streams raw grammar text; the OpenAI wire wants input_json
/// deltas of `{"<prop>":"<escaped prefix chunks>"}`.
#[derive(Clone, Debug, Default)]
pub struct GrammarToolInputJsonBuffer {
    pub input: String,
    pub started: bool,
    pub closed: bool,
}

impl GrammarToolInputJsonBuffer {
    pub fn append_delta(
        &mut self,
        input_property: &str,
        next_input: &str,
        close: bool,
    ) -> Result<Option<String>, String> {
        if self.closed {
            if close && next_input == self.input {
                return Ok(None);
            }
            return Err(format!(
                "grammar tool input for property \"{input_property}\" changed after it was closed"
            ));
        }
        if !next_input.starts_with(self.input.as_str()) {
            return Err(format!(
                "grammar tool input for property \"{input_property}\" changed non-monotonically"
            ));
        }
        let input_delta = &next_input[self.input.len()..];
        if !close && input_delta.is_empty() {
            return Ok(None);
        }

        let mut delta = String::new();
        if !self.started {
            delta.push_str(&format!("{{\"{input_property}\":\""));
            self.started = true;
        }
        // JSON-escape the chunk without the surrounding quotes.
        let escaped = serde_json::to_string(input_delta).map_err(|e| e.to_string())?;
        delta.push_str(&escaped[1..escaped.len() - 1]);
        self.input = next_input.to_string();

        if close {
            delta.push_str("\"}");
            self.closed = true;
        }
        Ok(Some(delta))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_conversion_makes_all_properties_required_and_nullable() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "count": { "type": "number" },
            },
            "required": ["path"],
        });
        let strict = make_strict_json_schema(&schema).unwrap();
        assert_eq!(strict["required"], json!(["count", "path"])); // serde maps sort keys
        assert_eq!(strict["additionalProperties"], json!(false));
        assert_eq!(strict["properties"]["path"], json!({ "type": "string" }));
        assert_eq!(
            strict["properties"]["count"]["anyOf"][1],
            json!({ "type": "null" })
        );
    }

    #[test]
    fn strict_conversion_rejects_unsupported_keys() {
        let schema = json!({ "type": "object", "properties": { "x": { "$ref": "#/$defs/x" } } });
        assert!(make_strict_json_schema(&schema).is_err());
        let schema =
            json!({ "type": "object", "properties": { "x": { "anyOf": [{ "type": "object" }] } } });
        assert!(make_strict_json_schema(&schema).is_err());
    }

    #[test]
    fn grammar_input_json_buffer_repackages() {
        let mut buffer = GrammarToolInputJsonBuffer::default();
        let d1 = buffer
            .append_delta("code", "fn main", false)
            .unwrap()
            .unwrap();
        assert_eq!(d1, "{\"code\":\"fn main");
        let d2 = buffer
            .append_delta("code", "fn main() {}", true)
            .unwrap()
            .unwrap();
        assert_eq!(d2, "() {}\"}");
        // A second append after close with identical input is a no-op.
        assert!(
            buffer
                .append_delta("code", "fn main() {}", true)
                .unwrap()
                .is_none()
        );
        // Non-monotonic input errors.
        let mut b2 = GrammarToolInputJsonBuffer::default();
        b2.append_delta("code", "abc", false).unwrap();
        assert!(b2.append_delta("code", "xyz", false).is_err());
    }

    #[test]
    fn grammar_resolution_requires_single_string_property() {
        let tool = crate::types::ToolDefinition {
            name: "run".to_string(),
            description: "run".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"],
            }),
            defer_loading: false,
            constrained_sampling: Some(ConstrainedSampling::Grammar {
                variants: GrammarVariants {
                    openai_lark: Some("start: .+".to_string()),
                    openai_regex: None,
                },
            }),
        };
        let grammar = resolve_grammar(&tool, true).unwrap().unwrap();
        assert_eq!(grammar.format, "lark");
        assert_eq!(grammar.input_property, "code");
        // Without provider support it falls back to a plain function tool.
        assert!(resolve_grammar(&tool, false).unwrap().is_none());
    }
}
