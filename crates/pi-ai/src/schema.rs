use anyhow::{Result, bail};
use regex::Regex;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::{LazyLock, Mutex};
///
/// Serializes to standard JSON Schema for providers. Property order is preserved
/// via [`Schema::property_order`] so serialization is deterministic (stable
/// provider prompt caches). Unknown keywords round-trip through [`Schema::extra`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Schema {
    pub schema_type: Option<Value>,
    pub description: Option<String>,
    pub properties: HashMap<String, Schema>,
    /// Insertion / source order of `properties` keys for deterministic serialization.
    pub property_order: Vec<String>,
    pub required: Vec<String>,
    pub items: Option<Box<Schema>>,
    pub enum_values: Vec<Value>,
    pub default: Option<Value>,
    /// Maps to JSON Schema `additionalProperties` (bool or nested schema).
    pub additional_properties: Option<Value>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub exclusive_minimum: Option<f64>,
    pub exclusive_maximum: Option<f64>,
    pub multiple_of: Option<f64>,
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
    pub pattern: Option<String>,
    pub min_items: Option<usize>,
    pub max_items: Option<usize>,
    /// JSON Schema `const`. `Some(Null)` is a real const-null (distinct from absent).
    pub const_value: Option<Value>,
    pub format: Option<String>,
    /// When true with a single primary type, serializes as `"type": [primary, "null"]`.
    pub nullable: bool,
    pub any_of: Vec<Schema>,
    pub one_of: Vec<Schema>,
    pub all_of: Vec<Schema>,
    pub extra: HashMap<String, Value>,
}

impl Schema {
    pub fn object(properties: HashMap<String, Schema>, required: Vec<String>) -> Self {
        let mut property_order: Vec<String> = properties.keys().cloned().collect();
        property_order.sort();
        Self {
            schema_type: Some(Value::String("object".into())),
            properties,
            property_order,
            required,
            ..Self::default()
        }
    }

    pub fn string() -> Self {
        Self {
            schema_type: Some(Value::String("string".into())),
            ..Self::default()
        }
    }

    /// Builds an object schema from ordered `(name, schema, required)` fields.
    /// Property serialization order matches `fields` order.
    pub fn object_ordered(fields: Vec<(String, Schema, bool)>) -> Self {
        let mut properties = HashMap::new();
        let mut property_order = Vec::with_capacity(fields.len());
        let mut required = Vec::new();
        for (name, schema, is_required) in fields {
            property_order.push(name.clone());
            if is_required {
                required.push(name.clone());
            }
            properties.insert(name, schema);
        }
        Self {
            schema_type: Some(Value::String("object".into())),
            properties,
            property_order,
            required,
            ..Self::default()
        }
    }

    pub fn validate(&self, value: &Value) -> Result<Value> {
        self.validate_at(value, "$")?;
        Ok(value.clone())
    }

    fn validate_at(&self, value: &Value, path: &str) -> Result<()> {
        for sub in &self.all_of {
            sub.validate_at(value, path)?;
        }

        if !self.any_of.is_empty()
            && !self
                .any_of
                .iter()
                .any(|s| s.validate_at(value, path).is_ok())
        {
            bail!("{path}: value does not match any schema");
        }

        if !self.one_of.is_empty() {
            let matches = self
                .one_of
                .iter()
                .filter(|s| s.validate_at(value, path).is_ok())
                .count();
            if matches != 1 {
                bail!("{path}: must match exactly one schema in oneOf");
            }
        }

        if let Some(c) = &self.const_value {
            if value != c {
                bail!("{path}: must be equal to constant");
            }
        }

        if !self.enum_values.is_empty() && !self.enum_values.contains(value) {
            bail!("{path}: value is not in enum");
        }

        let types = self.schema_types();
        if !types.is_empty() && !types.iter().any(|t| matches_json_type(value, t)) {
            bail!("{path}: expected {}", types.join(" or "));
        }

        match value {
            Value::Object(map) => {
                for name in &self.required {
                    if !map.contains_key(name) {
                        bail!("{path}.{name}: required property missing");
                    }
                }
                for (name, v) in map {
                    if let Some(s) = self.properties.get(name) {
                        s.validate_at(v, &format!("{path}.{name}"))?;
                    } else {
                        match &self.additional_properties {
                            Some(Value::Bool(false)) => {
                                bail!("{path}.{name}: additional property not allowed");
                            }
                            Some(schema_val) if schema_val.is_object() => {
                                let sub = schema_from_additional(schema_val);
                                sub.validate_at(v, &format!("{path}.{name}"))?;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Value::Array(values) => {
                if let Some(items) = &self.items {
                    for (i, v) in values.iter().enumerate() {
                        items.validate_at(v, &format!("{path}[{i}]"))?;
                    }
                }
                if let Some(n) = self.min_items {
                    if values.len() < n {
                        bail!("{path}: must not have fewer than {n} items");
                    }
                }
                if let Some(n) = self.max_items {
                    if values.len() > n {
                        bail!("{path}: must not have more than {n} items");
                    }
                }
            }
            Value::String(s) => {
                let char_len = s.chars().count();
                if let Some(n) = self.min_length {
                    if char_len < n {
                        bail!("{path}: shorter than {n}");
                    }
                }
                if let Some(n) = self.max_length {
                    if char_len > n {
                        bail!("{path}: longer than {n}");
                    }
                }
                if let Some(pat) = &self.pattern {
                    // Uncompilable patterns (e.g. lookaround) are skipped rather
                    // than failing every value — same policy as the Go port.
                    if let Some(re) = compiled_pattern(pat) {
                        if !re.is_match(s) {
                            bail!("{path}: must match pattern \"{pat}\"");
                        }
                    }
                }
            }
            _ => {
                if let Some(n) = as_finite_f64(value) {
                    if let Some(m) = self.minimum {
                        if n < m {
                            bail!("{path}: below minimum {m}");
                        }
                    }
                    if let Some(m) = self.maximum {
                        if n > m {
                            bail!("{path}: above maximum {m}");
                        }
                    }
                    if let Some(m) = self.exclusive_minimum {
                        if n <= m {
                            bail!("{path}: must be > {m}");
                        }
                    }
                    if let Some(m) = self.exclusive_maximum {
                        if n >= m {
                            bail!("{path}: must be < {m}");
                        }
                    }
                    if let Some(m) = self.multiple_of {
                        if !is_multiple_of(n, m) {
                            bail!("{path}: must be multiple of {m}");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn schema_types(&self) -> Vec<&str> {
        let mut types: Vec<&str> = match &self.schema_type {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if self.nullable {
            let has_null = types.iter().any(|t| *t == "null");
            if !has_null {
                // Prefer keeping a stable primary type then null (matches Go).
                if types.is_empty() {
                    types.push("null");
                } else if !types.contains(&"null") {
                    types.push("null");
                }
            }
        }
        types
    }
}

fn schema_from_additional(value: &Value) -> Schema {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

fn matches_json_type(value: &Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => as_finite_f64(value).is_some(),
        "integer" => as_finite_f64(value).is_some_and(|n| n.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn as_finite_f64(value: &Value) -> Option<f64> {
    let n = value.as_f64()?;
    if n.is_finite() { Some(n) } else { None }
}

/// TypeBox / JS-style multipleOf with 1e-10 tolerance and the integral-reciprocal shortcut.
fn is_multiple_of(dividend: f64, divisor: f64) -> bool {
    const TOLERANCE: f64 = 1e-10;
    if !dividend.is_finite() {
        return true;
    }
    if divisor == 0.0 {
        return false;
    }
    if dividend.fract() == 0.0 {
        let recip = 1.0 / divisor;
        if recip.fract() == 0.0 {
            return true;
        }
    }
    let m = dividend % divisor;
    m.abs().min((m - divisor).abs()) < TOLERANCE
}

fn compiled_pattern(pattern: &str) -> Option<Regex> {
    // Cache compiled patterns; failed compiles are remembered as None so
    // unenforceable patterns (e.g. lookaround) are skipped without error.
    static CACHE: LazyLock<Mutex<HashMap<String, Option<Regex>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = guard.get(pattern) {
        return entry.clone();
    }
    let compiled = Regex::new(pattern).ok();
    guard.insert(pattern.to_string(), compiled.clone());
    compiled
}

// ---------------------------------------------------------------------------
// Deterministic JSON Schema serialization / deserialization
// ---------------------------------------------------------------------------

impl Serialize for Schema {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        // Pre-count entries for SerializeMap.
        let mut count = 0usize;
        if self.schema_type.is_some() || self.nullable {
            count += 1;
        }
        if self.description.is_some() {
            count += 1;
        }
        let emit_object_keys = matches!(&self.schema_type, Some(Value::String(s)) if s == "object")
            || !self.properties.is_empty()
            || !self.property_order.is_empty();
        if emit_object_keys {
            count += 1; // properties
            if !self.required.is_empty() {
                count += 1;
            }
        }
        if self.items.is_some() {
            count += 1;
        }
        if !self.enum_values.is_empty() {
            count += 1;
        }
        if self.const_value.is_some() {
            count += 1;
        }
        if self.default.is_some() {
            count += 1;
        }
        if self.additional_properties.is_some() {
            count += 1;
        }
        if self.minimum.is_some() {
            count += 1;
        }
        if self.maximum.is_some() {
            count += 1;
        }
        if self.exclusive_minimum.is_some() {
            count += 1;
        }
        if self.exclusive_maximum.is_some() {
            count += 1;
        }
        if self.multiple_of.is_some() {
            count += 1;
        }
        if self.min_length.is_some() {
            count += 1;
        }
        if self.max_length.is_some() {
            count += 1;
        }
        if self.pattern.is_some() {
            count += 1;
        }
        if self.min_items.is_some() {
            count += 1;
        }
        if self.max_items.is_some() {
            count += 1;
        }
        if self.format.is_some() {
            count += 1;
        }
        if !self.any_of.is_empty() {
            count += 1;
        }
        if !self.one_of.is_empty() {
            count += 1;
        }
        if !self.all_of.is_empty() {
            count += 1;
        }
        count += self.extra.len();

        let mut map = serializer.serialize_map(Some(count))?;

        // type (nullable folds into [primary, "null"])
        if let Some(t) = &self.schema_type {
            if self.nullable {
                match t {
                    Value::String(primary) => {
                        let arr = Value::Array(vec![
                            Value::String(primary.clone()),
                            Value::String("null".into()),
                        ]);
                        map.serialize_entry("type", &arr)?;
                    }
                    Value::Array(arr) => {
                        let mut types = arr.clone();
                        if !types.iter().any(|v| v.as_str() == Some("null")) {
                            types.push(Value::String("null".into()));
                        }
                        map.serialize_entry("type", &Value::Array(types))?;
                    }
                    other => map.serialize_entry("type", other)?,
                }
            } else {
                map.serialize_entry("type", t)?;
            }
        } else if self.nullable {
            map.serialize_entry("type", &Value::String("null".into()))?;
        }

        if let Some(d) = &self.description {
            map.serialize_entry("description", d)?;
        }

        if emit_object_keys {
            let order = effective_property_order(self);
            // Serialize properties via an ordered map so key order is stable
            // even without serde_json's preserve_order feature.
            map.serialize_entry(
                "properties",
                &OrderedProperties {
                    order: &order,
                    properties: &self.properties,
                },
            )?;
            if !self.required.is_empty() {
                map.serialize_entry("required", &self.required)?;
            }
        }

        if let Some(items) = &self.items {
            map.serialize_entry("items", items.as_ref())?;
        }
        if !self.enum_values.is_empty() {
            map.serialize_entry("enum", &self.enum_values)?;
        }
        if let Some(c) = &self.const_value {
            map.serialize_entry("const", c)?;
        }
        if let Some(d) = &self.default {
            map.serialize_entry("default", d)?;
        }
        if let Some(ap) = &self.additional_properties {
            map.serialize_entry("additionalProperties", ap)?;
        }
        if let Some(v) = self.minimum {
            map.serialize_entry("minimum", &v)?;
        }
        if let Some(v) = self.maximum {
            map.serialize_entry("maximum", &v)?;
        }
        if let Some(v) = self.exclusive_minimum {
            map.serialize_entry("exclusiveMinimum", &v)?;
        }
        if let Some(v) = self.exclusive_maximum {
            map.serialize_entry("exclusiveMaximum", &v)?;
        }
        if let Some(v) = self.multiple_of {
            map.serialize_entry("multipleOf", &v)?;
        }
        if let Some(v) = self.min_length {
            map.serialize_entry("minLength", &v)?;
        }
        if let Some(v) = self.max_length {
            map.serialize_entry("maxLength", &v)?;
        }
        if let Some(p) = &self.pattern {
            map.serialize_entry("pattern", p)?;
        }
        if let Some(v) = self.min_items {
            map.serialize_entry("minItems", &v)?;
        }
        if let Some(v) = self.max_items {
            map.serialize_entry("maxItems", &v)?;
        }
        if let Some(f) = &self.format {
            map.serialize_entry("format", f)?;
        }
        if !self.any_of.is_empty() {
            map.serialize_entry("anyOf", &self.any_of)?;
        }
        if !self.one_of.is_empty() {
            map.serialize_entry("oneOf", &self.one_of)?;
        }
        if !self.all_of.is_empty() {
            map.serialize_entry("allOf", &self.all_of)?;
        }

        let mut extra_keys: Vec<&String> = self.extra.keys().collect();
        extra_keys.sort();
        for k in extra_keys {
            map.serialize_entry(k, &self.extra[k])?;
        }

        map.end()
    }
}

fn effective_property_order(schema: &Schema) -> Vec<String> {
    if !schema.property_order.is_empty() {
        return schema
            .property_order
            .iter()
            .filter(|k| schema.properties.contains_key(k.as_str()))
            .cloned()
            .collect();
    }
    let mut keys: Vec<String> = schema.properties.keys().cloned().collect();
    keys.sort();
    keys
}

/// Serializes a property map in a fixed key order.
struct OrderedProperties<'a> {
    order: &'a [String],
    properties: &'a HashMap<String, Schema>,
}

impl Serialize for OrderedProperties<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut seen = std::collections::HashSet::with_capacity(self.order.len());
        let mut len = 0usize;
        for name in self.order {
            if self.properties.contains_key(name) && seen.insert(name.as_str()) {
                len += 1;
            }
        }
        for name in self.properties.keys() {
            if !seen.contains(name.as_str()) {
                len += 1;
            }
        }
        let mut map = serializer.serialize_map(Some(len))?;
        seen.clear();
        for name in self.order {
            if let Some(schema) = self.properties.get(name) {
                if seen.insert(name.as_str()) {
                    map.serialize_entry(name, schema)?;
                }
            }
        }
        let mut rest: Vec<&String> = self
            .properties
            .keys()
            .filter(|k| !seen.contains(k.as_str()))
            .collect();
        rest.sort();
        for name in rest {
            if let Some(schema) = self.properties.get(name) {
                map.serialize_entry(name, schema)?;
            }
        }
        map.end()
    }
}

/// Deserializes a properties object while recording source key order.
struct OrderedPropertiesDe {
    properties: HashMap<String, Schema>,
    order: Vec<String>,
}

impl<'de> Deserialize<'de> for OrderedPropertiesDe {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct PropsVisitor;
        impl<'de> Visitor<'de> for PropsVisitor {
            type Value = OrderedPropertiesDe;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a properties object")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut access: M,
            ) -> std::result::Result<OrderedPropertiesDe, M::Error> {
                let mut properties = HashMap::new();
                let mut order = Vec::new();
                while let Some((key, value)) = access.next_entry::<String, Schema>()? {
                    order.push(key.clone());
                    properties.insert(key, value);
                }
                Ok(OrderedPropertiesDe { properties, order })
            }
        }
        deserializer.deserialize_map(PropsVisitor)
    }
}

impl<'de> Deserialize<'de> for Schema {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct SchemaVisitor;

        impl<'de> Visitor<'de> for SchemaVisitor {
            type Value = Schema;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON Schema object")
            }

            fn visit_map<M: MapAccess<'de>>(
                self,
                mut access: M,
            ) -> std::result::Result<Schema, M::Error> {
                let mut schema = Schema::default();
                let mut property_order_from_value: Option<Vec<String>> = None;

                while let Some(key) = access.next_key::<String>()? {
                    match key.as_str() {
                        "type" => {
                            let t: Value = access.next_value()?;
                            match t {
                                Value::String(s) => {
                                    if s == "null" {
                                        schema.nullable = true;
                                        schema.schema_type = Some(Value::String("null".into()));
                                    } else {
                                        schema.schema_type = Some(Value::String(s));
                                    }
                                }
                                Value::Array(arr) => {
                                    let mut primary: Option<String> = None;
                                    let mut has_null = false;
                                    let mut others = Vec::new();
                                    for item in arr {
                                        match item.as_str() {
                                            Some("null") => has_null = true,
                                            Some(s) => {
                                                if primary.is_none() {
                                                    primary = Some(s.to_string());
                                                } else {
                                                    others.push(Value::String(s.to_string()));
                                                }
                                            }
                                            _ => others.push(item),
                                        }
                                    }
                                    schema.nullable = has_null;
                                    if others.is_empty() {
                                        if let Some(p) = primary {
                                            schema.schema_type = Some(Value::String(p));
                                        } else if has_null {
                                            schema.schema_type = Some(Value::String("null".into()));
                                        }
                                    } else {
                                        let mut types = Vec::new();
                                        if let Some(p) = primary {
                                            types.push(Value::String(p));
                                        }
                                        types.extend(others);
                                        if has_null {
                                            types.push(Value::String("null".into()));
                                        }
                                        schema.schema_type = Some(Value::Array(types));
                                        // Multi-type arrays already encode null; avoid double-append on serialize.
                                        schema.nullable = false;
                                        if has_null {
                                            // Keep nullable semantics for schema_types() via the array.
                                            schema.nullable = false;
                                        }
                                    }
                                }
                                other => schema.schema_type = Some(other),
                            }
                        }
                        "description" => {
                            schema.description = Some(access.next_value()?);
                        }
                        "properties" => {
                            let props: OrderedPropertiesDe = access.next_value()?;
                            schema.properties = props.properties;
                            property_order_from_value = Some(props.order);
                        }
                        "required" => {
                            schema.required = access.next_value()?;
                        }
                        "items" => {
                            let item: Schema = access.next_value()?;
                            schema.items = Some(Box::new(item));
                        }
                        "enum" => {
                            schema.enum_values = access.next_value()?;
                        }
                        "const" => {
                            schema.const_value = Some(access.next_value()?);
                        }
                        "default" => {
                            schema.default = Some(access.next_value()?);
                        }
                        "additionalProperties" => {
                            schema.additional_properties = Some(access.next_value()?);
                        }
                        "minimum" => {
                            schema.minimum = Some(access.next_value()?);
                        }
                        "maximum" => {
                            schema.maximum = Some(access.next_value()?);
                        }
                        "exclusiveMinimum" => {
                            // Prefer draft-06+ numeric form; bool true is treated as absent bound.
                            let v: Value = access.next_value()?;
                            if let Some(n) = v.as_f64() {
                                schema.exclusive_minimum = Some(n);
                            } else if v.as_bool() == Some(true) {
                                // draft-04 exclusiveMinimum:true uses minimum as exclusive — leave to extra if needed.
                                schema.extra.insert("exclusiveMinimum".into(), v);
                            }
                        }
                        "exclusiveMaximum" => {
                            let v: Value = access.next_value()?;
                            if let Some(n) = v.as_f64() {
                                schema.exclusive_maximum = Some(n);
                            } else if v.as_bool() == Some(true) {
                                schema.extra.insert("exclusiveMaximum".into(), v);
                            }
                        }
                        "multipleOf" => {
                            schema.multiple_of = Some(access.next_value()?);
                        }
                        "minLength" => {
                            schema.min_length = Some(access.next_value()?);
                        }
                        "maxLength" => {
                            schema.max_length = Some(access.next_value()?);
                        }
                        "pattern" => {
                            schema.pattern = Some(access.next_value()?);
                        }
                        "minItems" => {
                            schema.min_items = Some(access.next_value()?);
                        }
                        "maxItems" => {
                            schema.max_items = Some(access.next_value()?);
                        }
                        "format" => {
                            schema.format = Some(access.next_value()?);
                        }
                        "nullable" => {
                            // OpenAPI-style nullable keyword.
                            let n: bool = access.next_value()?;
                            schema.nullable = schema.nullable || n;
                        }
                        "anyOf" => {
                            schema.any_of = access.next_value()?;
                        }
                        "oneOf" => {
                            schema.one_of = access.next_value()?;
                        }
                        "allOf" => {
                            schema.all_of = access.next_value()?;
                        }
                        other => {
                            let v: Value = access.next_value()?;
                            schema.extra.insert(other.to_string(), v);
                        }
                    }
                }

                if let Some(order) = property_order_from_value {
                    schema.property_order = order;
                }

                Ok(schema)
            }
        }

        deserializer.deserialize_map(SchemaVisitor)
    }
}

pub fn validate_tool_arguments(tool: &crate::ToolDefinition, arguments: &Value) -> Result<Value> {
    tool.parameters.validate(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_nested_required_properties_with_path() {
        let mut properties = HashMap::new();
        properties.insert("name".into(), Schema::string());
        let schema = Schema::object(properties, vec!["name".into()]);
        assert_eq!(
            schema.validate(&json!({"name": "pi"})).unwrap(),
            json!({"name": "pi"})
        );
        let error = schema.validate(&json!({})).unwrap_err().to_string();
        assert!(error.contains("$.name"), "{error}");
    }

    #[test]
    fn serializes_properties_in_deterministic_order() {
        let schema = Schema::object_ordered(vec![
            (
                "command".into(),
                {
                    let mut s = Schema::string();
                    s.description = Some("the shell command".into());
                    s
                },
                true,
            ),
            (
                "timeout".into(),
                {
                    let mut s = Schema {
                        schema_type: Some(Value::String("integer".into())),
                        description: Some("timeout in ms".into()),
                        ..Schema::default()
                    };
                    s
                },
                false,
            ),
        ]);
        let raw = serde_json::to_string(&schema).unwrap();
        let want = r#"{"type":"object","properties":{"command":{"type":"string","description":"the shell command"},"timeout":{"type":"integer","description":"timeout in ms"}},"required":["command"]}"#;
        assert_eq!(raw, want);
    }

    #[test]
    fn round_trip_preserves_property_order_and_keywords() {
        let src = r#"{"type":"object","properties":{"v":{"type":"number","minimum":1,"maximum":10,"exclusiveMinimum":0,"exclusiveMaximum":11,"multipleOf":0.5},"s":{"type":"string","minLength":1,"maxLength":4,"pattern":"^x","format":"email"},"a":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":3},"c":{"type":"string","const":"k"}},"required":["v"],"additionalProperties":true}"#;
        let schema: Schema = serde_json::from_str(src).unwrap();
        assert_eq!(
            schema.property_order,
            vec!["v", "s", "a", "c"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        let v = schema.properties.get("v").unwrap();
        assert_eq!(v.minimum, Some(1.0));
        assert_eq!(v.exclusive_minimum, Some(0.0));
        assert_eq!(v.multiple_of, Some(0.5));
        assert!(
            v.extra.is_empty(),
            "promoted keywords must not land in extra"
        );
        let s = schema.properties.get("s").unwrap();
        assert_eq!(s.pattern.as_deref(), Some("^x"));
        assert_eq!(s.format.as_deref(), Some("email"));
        let a = schema.properties.get("a").unwrap();
        assert_eq!(a.min_items, Some(1));
        assert_eq!(a.max_items, Some(3));
        assert_eq!(
            schema.properties.get("c").unwrap().const_value,
            Some(json!("k"))
        );
        assert_eq!(schema.additional_properties, Some(json!(true)));

        let raw = serde_json::to_string(&schema).unwrap();
        for key in [
            r#""minimum":1.0"#,
            r#""exclusiveMinimum":0.0"#,
            r#""multipleOf":0.5"#,
            r#""pattern":"^x""#,
            r#""format":"email""#,
            r#""minItems":1"#,
            r#""const":"k""#,
            r#""additionalProperties":true"#,
        ] {
            assert!(raw.contains(key), "round-trip lost {key}: {raw}");
        }
        // Property key order inside properties object.
        let v_pos = raw.find(r#""v":"#).unwrap();
        let s_pos = raw.find(r#""s":"#).unwrap();
        let a_pos = raw.find(r#""a":"#).unwrap();
        let c_pos = raw.find(r#""c":"#).unwrap();
        assert!(v_pos < s_pos && s_pos < a_pos && a_pos < c_pos, "{raw}");
    }

    #[test]
    fn nullable_type_round_trips_as_type_array() {
        let schema = Schema {
            schema_type: Some(Value::String("string".into())),
            nullable: true,
            ..Schema::default()
        };
        let raw = serde_json::to_string(&schema).unwrap();
        assert_eq!(raw, r#"{"type":["string","null"]}"#);
        let back: Schema = serde_json::from_str(&raw).unwrap();
        assert!(back.nullable);
        assert_eq!(back.schema_type, Some(Value::String("string".into())));
        assert!(back.validate(&json!(null)).is_ok());
        assert!(back.validate(&json!("ok")).is_ok());
        assert!(back.validate(&json!(1)).is_err());
    }

    #[test]
    fn openapi_nullable_keyword_deserializes() {
        let schema: Schema = serde_json::from_str(r#"{"type":"integer","nullable":true}"#).unwrap();
        assert!(schema.nullable);
        assert!(schema.validate(&json!(null)).is_ok());
        assert!(schema.validate(&json!(3)).is_ok());
    }

    #[test]
    fn validates_array_min_max_items() {
        let schema = Schema {
            schema_type: Some(Value::String("array".into())),
            items: Some(Box::new(Schema {
                schema_type: Some(Value::String("integer".into())),
                ..Schema::default()
            })),
            min_items: Some(2),
            max_items: Some(3),
            ..Schema::default()
        };
        assert!(schema.validate(&json!([1, 2])).is_ok());
        assert!(schema.validate(&json!([1])).is_err());
        assert!(schema.validate(&json!([1, 2, 3, 4])).is_err());
    }

    #[test]
    fn validates_numeric_bounds_multiple_of_and_exclusive() {
        let schema = Schema {
            schema_type: Some(Value::String("number".into())),
            minimum: Some(1.0),
            maximum: Some(10.0),
            exclusive_minimum: Some(1.0),
            exclusive_maximum: Some(10.0),
            multiple_of: Some(0.5),
            ..Schema::default()
        };
        assert!(schema.validate(&json!(1.5)).is_ok());
        assert!(schema.validate(&json!(1.0)).is_err()); // exclusive min
        assert!(schema.validate(&json!(10.0)).is_err()); // exclusive max
        assert!(schema.validate(&json!(1.25)).is_err()); // not multiple
        assert!(schema.validate(&json!(0.3)).is_ok() || schema.validate(&json!(9.0)).is_ok());
        assert!(schema.validate(&json!(9.0)).is_ok());
        // float multipleOf tolerance (0.3 / 0.1)
        let m = Schema {
            schema_type: Some(Value::String("number".into())),
            multiple_of: Some(0.1),
            ..Schema::default()
        };
        assert!(m.validate(&json!(0.3)).is_ok());
        assert!(m.validate(&json!(0.35)).is_err());
    }

    #[test]
    fn validates_const_including_null() {
        let schema = Schema {
            const_value: Some(json!("x")),
            schema_type: Some(Value::String("string".into())),
            ..Schema::default()
        };
        assert!(schema.validate(&json!("x")).is_ok());
        assert!(schema.validate(&json!("y")).is_err());

        let null_const = Schema {
            const_value: Some(Value::Null),
            schema_type: Some(Value::String("null".into())),
            ..Schema::default()
        };
        assert!(null_const.validate(&Value::Null).is_ok());
        assert!(null_const.validate(&json!(0)).is_err());
    }

    #[test]
    fn validates_one_of_and_all_of() {
        let one = Schema {
            one_of: vec![
                Schema {
                    schema_type: Some(Value::String("string".into())),
                    ..Schema::default()
                },
                Schema {
                    schema_type: Some(Value::String("integer".into())),
                    ..Schema::default()
                },
            ],
            ..Schema::default()
        };
        assert!(one.validate(&json!("a")).is_ok());
        assert!(one.validate(&json!(1)).is_ok());
        assert!(one.validate(&json!(true)).is_err());

        // Overlapping oneOf members: value matching both must fail.
        let overlap = Schema {
            one_of: vec![
                Schema {
                    schema_type: Some(Value::String("number".into())),
                    ..Schema::default()
                },
                Schema {
                    schema_type: Some(Value::String("integer".into())),
                    ..Schema::default()
                },
            ],
            ..Schema::default()
        };
        assert!(overlap.validate(&json!(1)).is_err());

        let all = Schema {
            all_of: vec![
                Schema {
                    schema_type: Some(Value::String("number".into())),
                    minimum: Some(0.0),
                    ..Schema::default()
                },
                Schema {
                    maximum: Some(10.0),
                    ..Schema::default()
                },
            ],
            ..Schema::default()
        };
        assert!(all.validate(&json!(5)).is_ok());
        assert!(all.validate(&json!(-1)).is_err());
        assert!(all.validate(&json!(11)).is_err());
    }

    #[test]
    fn validates_pattern_and_skips_unenforceable() {
        let schema = Schema {
            schema_type: Some(Value::String("string".into())),
            pattern: Some("^a+$".into()),
            ..Schema::default()
        };
        assert!(schema.validate(&json!("aaa")).is_ok());
        assert!(schema.validate(&json!("bbb")).is_err());

        // Lookahead is valid JS but often rejected by Rust regex — skip, do not error.
        let look = Schema {
            schema_type: Some(Value::String("string".into())),
            pattern: Some("^(?=.*a).*$".into()),
            ..Schema::default()
        };
        assert!(look.validate(&json!("anything")).is_ok());
    }

    #[test]
    fn additional_properties_true_allows_extras() {
        let mut properties = HashMap::new();
        properties.insert(
            "n".into(),
            Schema {
                schema_type: Some(Value::String("integer".into())),
                ..Schema::default()
            },
        );
        let mut schema = Schema::object(properties, vec!["n".into()]);
        schema.additional_properties = Some(json!(true));
        assert!(schema.validate(&json!({"n": 1, "extra": "ok"})).is_ok());

        schema.additional_properties = Some(json!(false));
        assert!(schema.validate(&json!({"n": 1, "extra": "nope"})).is_err());
    }

    #[test]
    fn object_constructor_still_accepts_hashmap() {
        let schema = Schema::object(
            HashMap::from([("target".into(), Schema::string())]),
            vec!["target".into()],
        );
        assert_eq!(schema.schema_type, Some(Value::String("object".into())));
        assert!(schema.properties.contains_key("target"));
    }
}
