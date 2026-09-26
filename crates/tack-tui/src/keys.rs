//! Keybinding registry: named actions mapped to key specs, with user
//! overrides from `<agentDir>/keybindings.json` (same schema as TS pi).

use std::collections::HashMap;
use std::path::Path;

use crate::input::KeyEvent;

#[derive(Clone, Debug)]
pub struct Keybindings {
    map: HashMap<String, Vec<String>>,
}

impl Keybindings {
    pub fn new() -> Self {
        Keybindings {
            map: HashMap::new(),
        }
    }

    /// Register default bindings for an action.
    pub fn register(&mut self, action: &str, specs: &[&str]) {
        self.map.insert(
            action.to_string(),
            specs.iter().map(|s| s.to_string()).collect(),
        );
    }

    /// Apply user overrides: `{ "action.name": "ctrl+x" | ["ctrl+x", ...] }`.
    /// An empty array clears the action's bindings.
    pub fn apply_overrides(&mut self, path: &Path) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
            return;
        };
        let Some(obj) = value.as_object() else { return };
        for (action, specs) in obj {
            let specs: Vec<String> = match specs {
                serde_json::Value::String(s) => vec![s.clone()],
                serde_json::Value::Array(a) => a
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                _ => continue,
            };
            self.map.insert(action.clone(), specs);
        }
    }

    /// True when the event matches any binding of the action.
    pub fn matches(&self, action: &str, event: &KeyEvent) -> bool {
        self.map
            .get(action)
            .is_some_and(|specs| specs.iter().any(|spec| event.matches(spec)))
    }

    pub fn specs(&self, action: &str) -> &[String] {
        self.map.get(action).map(Vec::as_slice).unwrap_or(&[])
    }
}

impl Default for Keybindings {
    fn default() -> Self {
        Self::new()
    }
}
