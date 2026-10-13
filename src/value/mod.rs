//! Dynamic JSON values and object helpers.
//!
//! The Go original carries its own `value.Value` / `value.Object` pair. Rust already has
//! one — [`serde_json::Value`] — so this module is a thin layer of accessors on top of
//! it rather than a parallel type hierarchy.

use serde_json::{Map, Value};

pub use serde_json::{Number, json};

use crate::web::{Error, Result};

/// A JSON object.
pub type Object = Map<String, Value>;

/// Parses a JSON document. An empty input yields `Value::Null` rather than an error.
pub fn decode_json(bytes: &[u8]) -> Result<Value> {
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(bytes).map_err(Error::from)
}

/// Reads a value at a dotted path, e.g. `web.server.port`.
pub fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Accessors for [`Object`] that coerce the way the Go original's `GetString`/`GetInt`
/// do: a value that is not the requested type is converted when that is unambiguous, and
/// yields the type's zero value when it is not.
pub trait ObjectExt {
    /// A string field. Numbers and booleans are rendered; `null` and missing give `""`.
    fn get_string(&self, key: &str) -> String;
    /// An `i64` field. Numeric strings are parsed.
    fn get_i64(&self, key: &str) -> i64;
    /// An `i64` field, or `default`.
    fn get_i64_or(&self, key: &str, default: i64) -> i64;
    /// An `f64` field.
    fn get_f64(&self, key: &str) -> f64;
    /// An `f64` field, or `default`.
    fn get_f64_or(&self, key: &str, default: f64) -> f64;
    /// A `bool` field.
    fn get_bool(&self, key: &str) -> bool;
    /// A `bool` field, or `default`.
    fn get_bool_or(&self, key: &str, default: bool) -> bool;
    /// A nested object.
    fn get_object(&self, key: &str) -> Option<&Object>;
    /// An array.
    fn get_array(&self, key: &str) -> Option<&Vec<Value>>;
    /// A string list, coercing scalar entries.
    fn get_string_list(&self, key: &str) -> Vec<String>;
    /// The raw value.
    fn value(&self, key: &str) -> Option<&Value>;
    /// Reports whether a key exists with a non-null value.
    fn has(&self, key: &str) -> bool;
}

impl ObjectExt for Object {
    fn get_string(&self, key: &str) -> String {
        match self.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        }
    }

    fn get_i64(&self, key: &str) -> i64 {
        self.get_i64_or(key, 0)
    }

    fn get_i64_or(&self, key: &str, default: i64) -> i64 {
        match self.get(key) {
            Some(Value::Number(n)) => n
                .as_i64()
                .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
            Some(Value::String(s)) => s.trim().parse().unwrap_or(default),
            Some(Value::Bool(b)) => i64::from(*b),
            _ => default,
        }
    }

    fn get_f64(&self, key: &str) -> f64 {
        self.get_f64_or(key, 0.0)
    }

    fn get_f64_or(&self, key: &str, default: f64) -> f64 {
        match self.get(key) {
            Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
            Some(Value::String(s)) => s.trim().parse().unwrap_or(default),
            _ => default,
        }
    }

    fn get_bool(&self, key: &str) -> bool {
        self.get_bool_or(key, false)
    }

    fn get_bool_or(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
            Some(Value::String(s)) => matches!(s.trim(), "true" | "1" | "yes" | "on"),
            _ => default,
        }
    }

    fn get_object(&self, key: &str) -> Option<&Object> {
        self.get(key).and_then(Value::as_object)
    }

    fn get_array(&self, key: &str) -> Option<&Vec<Value>> {
        self.get(key).and_then(Value::as_array)
    }

    fn get_string_list(&self, key: &str) -> Vec<String> {
        match self.get(key) {
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        }
    }

    fn value(&self, key: &str) -> Option<&Value> {
        self.get(key)
    }

    fn has(&self, key: &str) -> bool {
        matches!(self.get(key), Some(value) if !value.is_null())
    }
}

/// Deep-merges `patch` into `base`. Objects merge key by key; anything else is replaced.
pub fn merge(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(base_map), Value::Object(patch_map)) => {
            for (key, value) in patch_map {
                match base_map.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base_map.insert(key, value);
                    }
                }
            }
        }
        (base, patch) => *base = patch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_paths_walk_into_objects() {
        let root = json!({"web": {"server": {"port": 8080}}});
        assert_eq!(get_path(&root, "web.server.port"), Some(&json!(8080)));
        assert_eq!(get_path(&root, "web.missing"), None);
        assert_eq!(get_path(&root, "web.server.port.deep"), None);
    }

    #[test]
    fn object_accessors_coerce_like_the_go_original() {
        let object = json!({
            "name": "alice",
            "age": "31",
            "score": 9.5,
            "admin": "true",
            "tags": ["a", "b"],
            "nested": {"x": 1},
            "nothing": null
        })
        .as_object()
        .unwrap()
        .clone();

        assert_eq!(object.get_string("name"), "alice");
        assert_eq!(object.get_i64("age"), 31);
        assert_eq!(object.get_f64("score"), 9.5);
        assert!(object.get_bool("admin"));
        assert_eq!(object.get_string_list("tags"), vec!["a", "b"]);
        assert_eq!(object.get_i64_or("missing", 5), 5);
        assert_eq!(object.get_string("nothing"), "");
        assert!(!object.has("nothing"));
        assert_eq!(object.get_object("nested").unwrap().get_i64("x"), 1);
    }

    #[test]
    fn merging_is_deep_for_objects_and_replacing_otherwise() {
        let mut base = json!({"a": 1, "nested": {"x": 1, "y": 2}});
        merge(&mut base, json!({"b": 2, "nested": {"y": 9}}));
        assert_eq!(base, json!({"a": 1, "b": 2, "nested": {"x": 1, "y": 9}}));

        merge(&mut base, json!({"a": {"now": "an object"}}));
        assert_eq!(base["a"], json!({"now": "an object"}));
    }

    #[test]
    fn empty_bodies_decode_to_null() {
        assert_eq!(decode_json(b"").unwrap(), Value::Null);
        assert!(decode_json(b"{").is_err());
    }
}
