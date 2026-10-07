//! A custom resource's structural schema applied to the object (#121): the
//! CRD version's `openAPIV3Schema` decides what a custom resource may hold.
//!
//! - **Defaulting**: a property absent from the object (or `null` where the
//!   schema is not `nullable`) gets the schema's `default`, at every depth.
//!   Run on every write and on every read, so a default added to the CRD
//!   later shows on objects stored before it, as upstream defaults "from
//!   storage".
//! - **Pruning**: a property the schema does not declare is removed, unless
//!   the node says `x-kubernetes-preserve-unknown-fields: true` or allows
//!   `additionalProperties`. At the root, and in an object marked
//!   `x-kubernetes-embedded-resource`, `apiVersion`, `kind` and `metadata`
//!   are implied, and `metadata` is pruned to ObjectMeta's own fields.
//! - **Unknown fields**: every pruned path, in upstream's spelling
//!   (`.spec.foo`, `.spec.ports[0].bar`), for `fieldValidation`: `Strict`
//!   refuses the request with them, `Warn` (the default) returns them as
//!   warnings, `Ignore` says nothing. They are pruned either way.
//!
//! Types, formats, patterns and other validation are not checked here.

use serde_json::{Map, Value};

/// ObjectMeta's fields: what `metadata` keeps at the root and in an embedded
/// resource.
const OBJECT_META: &[&str] = &[
    "name", "generateName", "namespace", "selfLink", "uid", "resourceVersion",
    "generation", "creationTimestamp", "deletionTimestamp", "deletionGracePeriodSeconds",
    "labels", "annotations", "ownerReferences", "finalizers", "managedFields",
];

/// What to do with a request's undeclared fields (`?fieldValidation=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FieldValidation {
    Ignore,
    #[default]
    Warn,
    Strict,
}

impl FieldValidation {
    /// From the request's query string; absent or unknown is `Warn`, the
    /// server's default.
    pub fn from_query(query: &str) -> Self {
        for (k, v) in form_urlencoded::parse(query.as_bytes()) {
            if k == "fieldValidation" {
                return match v.as_ref() {
                    "Strict" => FieldValidation::Strict,
                    "Ignore" => FieldValidation::Ignore,
                    _ => FieldValidation::Warn,
                };
            }
        }
        FieldValidation::Warn
    }
}

/// Default and prune `obj` (a whole custom resource) under `schema` (the
/// version's `openAPIV3Schema`). Returns the paths pruned, in order.
pub fn apply(obj: &mut Value, schema: &Value) -> Vec<String> {
    let mut unknown = Vec::new();
    walk(obj, schema, "", true, &mut unknown);
    unknown
}

/// Defaults only — a read from storage, which must not drop what a client
/// stored under an older schema.
pub fn default_only(obj: &mut Value, schema: &Value) {
    defaults(obj, schema);
}

fn defaults(obj: &mut Value, schema: &Value) {
    match obj {
        Value::Object(map) => {
            if let Some(props) = schema["properties"].as_object() {
                for (k, s) in props {
                    fill_default(map, k, s);
                    if let Some(v) = map.get_mut(k) {
                        defaults(v, s);
                    }
                }
            }
            if let Some(extra) = schema.get("additionalProperties").filter(|v| v.is_object()) {
                let declared = schema["properties"].as_object();
                for (k, v) in map.iter_mut() {
                    if !declared.is_some_and(|d| d.contains_key(k)) {
                        defaults(v, extra);
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(s) = schema.get("items") {
                for v in items {
                    defaults(v, s);
                }
            }
        }
        _ => {}
    }
}

fn fill_default(map: &mut Map<String, Value>, key: &str, schema: &Value) {
    let Some(default) = schema.get("default") else { return };
    let nullable = schema["nullable"].as_bool().unwrap_or(false);
    match map.get(key) {
        None => {
            map.insert(key.to_string(), default.clone());
        }
        Some(Value::Null) if !nullable => {
            map.insert(key.to_string(), default.clone());
        }
        _ => {}
    }
}

fn walk(obj: &mut Value, schema: &Value, path: &str, resource: bool, unknown: &mut Vec<String>) {
    match obj {
        Value::Object(map) => {
            let props = schema["properties"].as_object();
            let preserve = schema["x-kubernetes-preserve-unknown-fields"].as_bool().unwrap_or(false);
            let extra = schema.get("additionalProperties");
            let embedded = resource || schema["x-kubernetes-embedded-resource"].as_bool().unwrap_or(false);
            if let Some(props) = props {
                for (k, s) in props {
                    fill_default(map, k, s);
                }
            }
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                let child = format!("{path}.{k}");
                if embedded && (k == "apiVersion" || k == "kind") {
                    continue;
                }
                if embedded && k == "metadata" {
                    if let Some(Value::Object(meta)) = map.get_mut(&k) {
                        let extra_meta: Vec<String> =
                            meta.keys().filter(|f| !OBJECT_META.contains(&f.as_str())).cloned().collect();
                        for f in extra_meta {
                            meta.remove(&f);
                            unknown.push(format!("{child}.{f}"));
                        }
                    }
                    continue;
                }
                if let Some(s) = props.and_then(|p| p.get(&k)) {
                    if let Some(v) = map.get_mut(&k) {
                        walk(v, s, &child, false, unknown);
                    }
                    continue;
                }
                match extra {
                    Some(Value::Bool(true)) => continue,
                    Some(s @ Value::Object(_)) => {
                        if let Some(v) = map.get_mut(&k) {
                            walk(v, s, &child, false, unknown);
                        }
                        continue;
                    }
                    _ => {}
                }
                if preserve {
                    continue;
                }
                map.remove(&k);
                unknown.push(child);
            }
        }
        Value::Array(items) => {
            if let Some(s) = schema.get("items") {
                for (i, v) in items.iter_mut().enumerate() {
                    walk(v, s, &format!("{path}[{i}]"), false, unknown);
                }
            }
        }
        _ => {}
    }
}

/// Every key repeated within one JSON object of `body`, as `a.b` paths
/// (`fieldValidation=Strict` refuses them; `serde_json::Value` would keep
/// only the last). `Err` only for a body that is not JSON at all.
pub fn json_duplicates(body: &[u8]) -> Result<Vec<String>, serde_json::Error> {
    use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
    struct Seed<'a> {
        path: String,
        dups: &'a std::cell::RefCell<Vec<String>>,
    }
    impl<'de> DeserializeSeed<'de> for Seed<'_> {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for Seed<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("JSON")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<(), A::Error> {
            let mut seen = std::collections::HashSet::new();
            while let Some(k) = m.next_key::<String>()? {
                let path = if self.path.is_empty() { k.clone() } else { format!("{}.{k}", self.path) };
                if !seen.insert(k) {
                    self.dups.borrow_mut().push(path.clone());
                }
                m.next_value_seed(Seed { path, dups: self.dups })?;
            }
            Ok(())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<(), A::Error> {
            let mut i = 0;
            while s.next_element_seed(Seed { path: format!("{}[{i}]", self.path), dups: self.dups })?.is_some() {
                i += 1;
            }
            Ok(())
        }
        fn visit_bool<E>(self, _: bool) -> Result<(), E> { Ok(()) }
        fn visit_i64<E>(self, _: i64) -> Result<(), E> { Ok(()) }
        fn visit_u64<E>(self, _: u64) -> Result<(), E> { Ok(()) }
        fn visit_f64<E>(self, _: f64) -> Result<(), E> { Ok(()) }
        fn visit_str<E>(self, _: &str) -> Result<(), E> { Ok(()) }
        fn visit_unit<E>(self) -> Result<(), E> { Ok(()) }
    }
    let dups = std::cell::RefCell::new(Vec::new());
    let mut de = serde_json::Deserializer::from_slice(body);
    Seed { path: String::new(), dups: &dups }.deserialize(&mut de)?;
    Ok(dups.into_inner())
}

/// The first key repeated within one mapping of a YAML `body`, in the words
/// of upstream's YAML decoder: `line 9: key "foo" already set in map`.
/// Line numbers are 1-based, counting the body's leading newline.
///
/// A scan of block mappings by indentation (a `- ` item starts a new one),
/// enough for the manifests clients apply; flow mappings (`{a: 1}`) and
/// multi-line block scalars are not looked into.
pub fn yaml_duplicate(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    // (indentation, keys seen) for each open mapping, innermost last.
    let mut stack: Vec<(usize, std::collections::HashSet<String>)> = Vec::new();
    let mut scalar_indent: Option<usize> = None; // inside `key: |` / `key: >`
    for (i, line) in text.split('\n').enumerate() {
        let trimmed = line.trim_start();
        let mut indent = line.len() - trimmed.len();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" {
            continue;
        }
        if let Some(si) = scalar_indent {
            if indent > si {
                continue;
            }
            scalar_indent = None;
        }
        let mut rest = trimmed;
        let mut new_item = false;
        while let Some(r) = rest.strip_prefix("- ") {
            indent += 2;
            rest = r.trim_start();
            new_item = true;
        }
        while stack.last().is_some_and(|(ind, _)| *ind > indent || (new_item && *ind == indent)) {
            stack.pop();
        }
        let Some((raw_key, value)) = split_key(rest) else { continue };
        let key = raw_key.trim_matches('"').trim_matches('\'').to_string();
        if value.starts_with('|') || value.starts_with('>') {
            scalar_indent = Some(indent);
        }
        match stack.last_mut() {
            Some((ind, keys)) if *ind == indent => {
                if !keys.insert(key.clone()) {
                    return Some(format!("line {}: key \"{key}\" already set in map", i + 1));
                }
            }
            _ => stack.push((indent, std::iter::once(key).collect())),
        }
    }
    None
}

/// `key: value` → (key, value): the first `:` followed by a space or the end
/// of the line, outside quotes.
fn split_key(s: &str) -> Option<(&str, &str)> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') if i == 0 => quote = Some(c),
            (None, ':') => {
                let after = &s[i + 1..];
                if after.is_empty() || after.starts_with(' ') {
                    return Some((&s[..i], after.trim()));
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ports_schema(preserve: bool) -> Value {
        json!({"type": "object", "properties": {"spec": {"type": "object",
            "x-kubernetes-preserve-unknown-fields": preserve,
            "properties": {"foo": {"type": "string"},
                "ports": {"type": "array", "items": {"type": "object",
                    "properties": {"containerPort": {"type": "integer"}, "protocol": {"type": "string", "default": "TCP"}}}}}}}})
    }

    #[test]
    fn unknown_fields_are_pruned_and_reported_in_upstreams_spelling() {
        let mut cr = json!({"apiVersion": "x.io/v1", "kind": "X",
            "metadata": {"name": "a", "unknownMeta": "u"},
            "unknownField": "u",
            "spec": {"foo": "f", "bar": 1, "ports": [{"containerPort": 80, "hostX": 1}]}});
        let unknown = apply(&mut cr, &ports_schema(false));
        assert_eq!(unknown, [".metadata.unknownMeta", ".spec.bar", ".spec.ports[0].hostX", ".unknownField"]);
        assert_eq!(cr, json!({"apiVersion": "x.io/v1", "kind": "X", "metadata": {"name": "a"},
            "spec": {"foo": "f", "ports": [{"containerPort": 80, "protocol": "TCP"}]}}));
    }

    #[test]
    fn preserve_unknown_fields_keeps_them() {
        let mut cr = json!({"spec": {"unknown": "u", "foo": "f"}});
        assert!(apply(&mut cr, &ports_schema(true)).is_empty());
        assert_eq!(cr["spec"]["unknown"], "u");
    }

    #[test]
    fn an_embedded_resource_keeps_its_type_and_prunes_its_metadata() {
        let schema = json!({"type": "object", "properties": {"spec": {"type": "object",
            "x-kubernetes-preserve-unknown-fields": true, "properties": {"template": {"type": "object",
                "x-kubernetes-embedded-resource": true, "x-kubernetes-preserve-unknown-fields": true,
                "properties": {"spec": {"type": "object"}}}}}}});
        let mut cr = json!({"metadata": {"name": "a"}, "spec": {"template": {
            "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "t", "unknownSubMeta": "u"}, "spec": {}}}});
        assert_eq!(apply(&mut cr, &schema), [".spec.template.metadata.unknownSubMeta"]);
        assert_eq!(cr["spec"]["template"]["kind"], "Pod");
        assert!(cr["spec"]["template"]["metadata"].get("unknownSubMeta").is_none());
    }

    #[test]
    fn additional_properties_are_kept_and_walked() {
        let schema = json!({"type": "object", "properties": {"spec": {"type": "object",
            "additionalProperties": {"type": "object", "properties": {"n": {"type": "integer", "default": 1}}}}}});
        let mut cr = json!({"spec": {"a": {}, "b": {"n": 5, "x": 1}}});
        assert_eq!(apply(&mut cr, &schema), [".spec.b.x"]);
        assert_eq!(cr["spec"], json!({"a": {"n": 1}, "b": {"n": 5}}));
    }

    #[test]
    fn defaults_on_read_fill_without_pruning() {
        let schema = json!({"type": "object", "properties": {"spec": {"type": "object", "default": {},
            "properties": {"a": {"type": "string", "default": "A"}, "b": {"type": "string", "default": "B", "nullable": true},
                           "c": {"type": "string", "default": "C"}}}}});
        let mut stored = json!({"spec": {"a": "mine", "b": null, "c": null}, "unknown": 1});
        default_only(&mut stored, &schema);
        assert_eq!(stored, json!({"spec": {"a": "mine", "b": null, "c": "C"}, "unknown": 1}));
        let mut empty = json!({});
        default_only(&mut empty, &schema);
        assert_eq!(empty["spec"], json!({"a": "A", "b": "B", "c": "C"}));
    }

    #[test]
    fn field_validation_from_the_query() {
        assert_eq!(FieldValidation::from_query("fieldManager=m&fieldValidation=Strict"), FieldValidation::Strict);
        assert_eq!(FieldValidation::from_query("fieldValidation=Ignore"), FieldValidation::Ignore);
        assert_eq!(FieldValidation::from_query(""), FieldValidation::Warn);
    }

    #[test]
    fn duplicates_are_found_before_they_collapse() {
        assert_eq!(json_duplicates(br#"{"spec":{"foo":1,"bar":2,"foo":3},"x":[{"a":1,"a":2}]}"#).unwrap(),
                   ["spec.foo", "x[0].a"]);
        assert!(json_duplicates(br#"{"a":{"b":1},"c":{"b":2}}"#).unwrap().is_empty());
        // The conformance spec's body: the duplicate is on line 9.
        let yaml = b"\napiVersion: x.io/v1\nkind: X\nmetadata:\n  name: a\nspec:\n  unknown: uk1\n  foo: foo1\n  foo: foo2\n  cronSpec: x\n";
        assert_eq!(yaml_duplicate(yaml).as_deref(), Some("line 9: key \"foo\" already set in map"));
        assert_eq!(yaml_duplicate(b"a: 1\nb: 2\n"), None);
        // The same key in sibling list items, or nested maps, is no repeat.
        assert_eq!(yaml_duplicate(b"ports:\n- name: a\n  port: 1\n- name: b\n  port: 2\nx:\n  name: c\n"), None);
        // A block scalar's lines are not keys.
        assert_eq!(yaml_duplicate(b"data:\n  a: |\n    a: 1\n    a: 2\n  b: x\n"), None);
        assert_eq!(yaml_duplicate(b"- a: 1\n  a: 2\n").as_deref(), Some("line 2: key \"a\" already set in map"));
    }
}
