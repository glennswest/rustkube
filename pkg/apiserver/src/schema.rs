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
//! - **Nulls**: a declared property set to `null` where the schema is not
//!   `nullable` is dropped (then defaulted), as upstream prunes them.
//! - **Validation** (`validate`): the value validations of a structural
//!   schema on the pruned, defaulted object — `type` (with
//!   `x-kubernetes-int-or-string`), `required`, `enum`, `pattern`,
//!   `min/maxLength`, `minimum`/`maximum` (exclusive too), `multipleOf`,
//!   `min/maxItems`, `min/maxProperties`, and `x-kubernetes-list-type`
//!   `set` / `map` (by `x-kubernetes-list-map-keys`) uniqueness. Errors are
//!   upstream's field errors (`spec.x: Required value`, …); on an update an
//!   error on a value the stored object already had is dropped, as
//!   upstream's validation ratcheting does.
//!
//! Not here: `format`, `allOf`/`anyOf`/`oneOf`/`not`, and CEL
//! `x-kubernetes-validations` (which need a CEL engine, #234).

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
    let nullable = schema["nullable"].as_bool().unwrap_or(false);
    let Some(default) = schema.get("default") else {
        // Upstream prunes a null it may not hold before defaulting.
        if !nullable && map.get(key).is_some_and(Value::is_null) {
            map.remove(key);
        }
        return;
    };
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

/// One field error, upstream's way: the field's path (`spec.ports[1]`) and
/// what is wrong with it, plus where it is, for ratcheting.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldError {
    pub field: String,
    pub detail: String,
    pointer: Vec<Seg>,
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.detail)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Key(String),
    Index(usize),
}

/// The value validations of `schema` on `obj` (a whole custom resource,
/// already pruned and defaulted). `old` is the stored object on an update:
/// an error on a value it already held is dropped (upstream's ratcheting:
/// an unchanged field written under an older schema stays writable). A
/// required field missing now and before stays an error only if it was
/// there before.
pub fn validate(obj: &Value, schema: &Value, old: Option<&Value>) -> Vec<FieldError> {
    let mut errs = Vec::new();
    check(obj, schema, &mut Vec::new(), true, &mut errs);
    if let Some(old) = old {
        errs.retain(|e| {
            let (now, before) = (at(obj, &e.pointer), at(old, &e.pointer));
            if e.detail == "Required value" {
                before.is_some()
            } else {
                now != before
            }
        });
    }
    errs
}

/// `Kind.group "name" is invalid: …`, upstream's 422 message for `errs`.
pub fn invalid_message(obj: &Value, errs: &[FieldError]) -> String {
    let kind = obj["kind"].as_str().unwrap_or("");
    let group = obj["apiVersion"].as_str().and_then(|v| v.rsplit_once('/')).map_or("", |(g, _)| g);
    let name = obj["metadata"]["name"].as_str().unwrap_or("");
    let list: Vec<String> = errs.iter().map(ToString::to_string).collect();
    let what = if list.len() == 1 { list[0].clone() } else { format!("[{}]", list.join(", ")) };
    format!("{kind}.{group} \"{name}\" is invalid: {what}")
}

fn at<'a>(v: &'a Value, pointer: &[Seg]) -> Option<&'a Value> {
    pointer.iter().try_fold(v, |v, s| match s {
        Seg::Key(k) => v.get(k),
        Seg::Index(i) => v.get(*i),
    })
}

fn path_of(pointer: &[Seg]) -> String {
    let mut out = String::new();
    for s in pointer {
        match s {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

/// A value as upstream prints it in an error: strings quoted, the rest JSON.
fn shown(v: &Value) -> String {
    match v {
        Value::String(s) => format!("{s:?}"),
        v => v.to_string(),
    }
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn push(errs: &mut Vec<FieldError>, ptr: &[Seg], detail: String) {
    errs.push(FieldError { field: path_of(ptr), detail, pointer: ptr.to_vec() });
}

fn check(v: &Value, schema: &Value, ptr: &mut Vec<Seg>, resource: bool, errs: &mut Vec<FieldError>) {
    let path = path_of(ptr);
    // The type first: nothing else applies to a value of the wrong type.
    let int_or_string = schema["x-kubernetes-int-or-string"].as_bool().unwrap_or(false);
    let nullable = schema["nullable"].as_bool().unwrap_or(false);
    let got = type_of(v);
    if v.is_null() {
        if !nullable && (schema["type"].is_string() || int_or_string) {
            let want = schema["type"].as_str().unwrap_or("integer,string");
            push(errs, ptr, format!("Invalid value: \"null\": {path} in body must be of type {want}: \"null\""));
        }
        return;
    }
    if int_or_string {
        if got != "string" && got != "integer" {
            push(errs, ptr, format!("Invalid value: \"{got}\": {path} in body must be of type integer,string: \"{got}\""));
            return;
        }
    } else if let Some(want) = schema["type"].as_str() {
        if got != want && !(want == "number" && got == "integer") {
            push(errs, ptr, format!("Invalid value: \"{got}\": {path} in body must be of type {want}: \"{got}\""));
            return;
        }
    }
    if let Some(allowed) = schema["enum"].as_array() {
        if !allowed.contains(v) {
            let list: Vec<String> = allowed.iter().map(shown).collect();
            push(errs, ptr, format!("Unsupported value: {}: supported values: {}", shown(v), list.join(", ")));
        }
    }
    match v {
        Value::String(s) => {
            let len = s.chars().count() as u64;
            if let Some(max) = schema["maxLength"].as_u64().filter(|m| len > *m) {
                push(errs, ptr, format!("Too long: may not be more than {max} bytes"));
            }
            if let Some(min) = schema["minLength"].as_u64().filter(|m| len < *m) {
                push(errs, ptr, format!("Invalid value: {}: {path} in body should be at least {min} chars long", shown(v)));
            }
            if let Some(pat) = schema["pattern"].as_str() {
                // An unparseable pattern is the CRD's fault (upstream refuses
                // such a CRD), so there is nothing to hold the value to.
                if regex::Regex::new(pat).is_ok_and(|re| !re.is_match(s)) {
                    push(errs, ptr, format!("Invalid value: {}: {path} in body should match '{pat}'", shown(v)));
                }
            }
        }
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            let excl = |k: &str| schema[k].as_bool().unwrap_or(false);
            if let Some(max) = schema["maximum"].as_f64() {
                if excl("exclusiveMaximum") && x >= max {
                    push(errs, ptr, format!("Invalid value: {n}: {path} in body should be less than {}", schema["maximum"]));
                } else if x > max {
                    push(errs, ptr, format!("Invalid value: {n}: {path} in body should be less than or equal to {}", schema["maximum"]));
                }
            }
            if let Some(min) = schema["minimum"].as_f64() {
                if excl("exclusiveMinimum") && x <= min {
                    push(errs, ptr, format!("Invalid value: {n}: {path} in body should be greater than {}", schema["minimum"]));
                } else if x < min {
                    push(errs, ptr, format!("Invalid value: {n}: {path} in body should be greater than or equal to {}", schema["minimum"]));
                }
            }
            if let Some(m) = schema["multipleOf"].as_f64().filter(|m| *m > 0.0) {
                if ((x / m) - (x / m).round()).abs() > 1e-9 {
                    push(errs, ptr, format!("Invalid value: {n}: {path} in body should be a multiple of {}", schema["multipleOf"]));
                }
            }
        }
        Value::Array(items) => {
            let n = items.len() as u64;
            if let Some(max) = schema["maxItems"].as_u64().filter(|m| n > *m) {
                push(errs, ptr, format!("Too many: {n}: must have at most {max} items"));
            }
            if let Some(min) = schema["minItems"].as_u64().filter(|m| n < *m) {
                push(errs, ptr, format!("Invalid value: {n}: {path} in body should have at least {min} items"));
            }
            match schema["x-kubernetes-list-type"].as_str() {
                Some("set") => {
                    for (i, item) in items.iter().enumerate() {
                        if items[..i].contains(item) {
                            ptr.push(Seg::Index(i));
                            push(errs, ptr, format!("Duplicate value: {}", shown(item)));
                            ptr.pop();
                        }
                    }
                }
                Some("map") => {
                    let keys: Vec<&str> = schema["x-kubernetes-list-map-keys"]
                        .as_array()
                        .map(|k| k.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    let key_of = |item: &Value| -> Vec<Value> {
                        keys.iter().map(|k| item.get(*k).cloned().unwrap_or(Value::Null)).collect()
                    };
                    for (i, item) in items.iter().enumerate() {
                        let k = key_of(item);
                        if items[..i].iter().any(|o| key_of(o) == k) {
                            let pairs: Vec<String> =
                                keys.iter().zip(&k).map(|(name, v)| format!("{name:?}:{}", shown(v))).collect();
                            ptr.push(Seg::Index(i));
                            push(errs, ptr, format!("Duplicate value: map[string]interface {{}}{{{}}}", pairs.join(", ")));
                            ptr.pop();
                        }
                    }
                }
                _ => {}
            }
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    ptr.push(Seg::Index(i));
                    check(item, item_schema, ptr, false, errs);
                    ptr.pop();
                }
            }
        }
        Value::Object(map) => {
            let embedded = resource || schema["x-kubernetes-embedded-resource"].as_bool().unwrap_or(false);
            let n = map.len() as u64;
            if let Some(max) = schema["maxProperties"].as_u64().filter(|m| n > *m) {
                push(errs, ptr, format!("Too many: {n}: must have at most {max} items"));
            }
            if let Some(min) = schema["minProperties"].as_u64().filter(|m| n < *m) {
                push(errs, ptr, format!("Invalid value: {n}: {path} in body should have at least {min} properties"));
            }
            for r in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if !map.contains_key(r) {
                    ptr.push(Seg::Key(r.to_string()));
                    push(errs, ptr, "Required value".to_string());
                    ptr.pop();
                }
            }
            let props = schema["properties"].as_object();
            let extra = schema.get("additionalProperties").filter(|v| v.is_object());
            for (k, child) in map {
                // The root's and an embedded resource's identity are the
                // apiserver's to check, not the schema's.
                if embedded && matches!(k.as_str(), "apiVersion" | "kind" | "metadata") {
                    continue;
                }
                let Some(s) = props.and_then(|p| p.get(k)).or(extra) else { continue };
                ptr.push(Seg::Key(k.clone()));
                check(child, s, ptr, false, errs);
                ptr.pop();
            }
        }
        Value::Bool(_) | Value::Null => {}
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

    /// The flowsdn audit's shapes (#121): a required spec, an enum with a
    /// default, bounds, a pattern, a keyed list and a set.
    fn policy_schema() -> Value {
        json!({"type": "object", "required": ["spec"], "properties": {"spec": {"type": "object",
            "required": ["selector"],
            "properties": {
                "selector": {"type": "string", "pattern": "^[a-z]+$", "maxLength": 8},
                "family": {"type": "string", "enum": ["IPv4", "IPv6"], "default": "IPv4"},
                "priority": {"type": "integer", "minimum": 1, "maximum": 10},
                "weight": {"type": "number", "exclusiveMinimum": true, "minimum": 0, "multipleOf": 0.5},
                "port": {"x-kubernetes-int-or-string": true},
                "note": {"type": "string", "nullable": true},
                "rules": {"type": "array", "maxItems": 3, "x-kubernetes-list-type": "map",
                    "x-kubernetes-list-map-keys": ["name", "protocol"],
                    "items": {"type": "object", "required": ["name"],
                        "properties": {"name": {"type": "string"}, "protocol": {"type": "string", "default": "TCP"}}}},
                "tags": {"type": "array", "x-kubernetes-list-type": "set", "items": {"type": "string"}},
                "labels": {"type": "object", "maxProperties": 2, "additionalProperties": {"type": "string"}}}}}})
    }

    fn errors(mut cr: Value, old: Option<&Value>) -> Vec<String> {
        apply(&mut cr, &policy_schema());
        validate(&cr, &policy_schema(), old).iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_valid_object_has_no_errors_and_gets_its_defaults() {
        let mut cr = json!({"apiVersion": "net.io/v1", "kind": "P", "metadata": {"name": "a"},
            "spec": {"selector": "web", "priority": 5, "weight": 1.5, "port": "http", "note": null,
                     "rules": [{"name": "a"}, {"name": "a", "protocol": "UDP"}], "tags": ["x", "y"],
                     "labels": {"k": "v"}}});
        apply(&mut cr, &policy_schema());
        assert_eq!(cr["spec"]["family"], "IPv4", "C4: the omitted family is defaulted");
        assert_eq!(cr["spec"]["rules"][0]["protocol"], "TCP");
        assert!(cr["spec"]["note"].is_null() && cr["spec"].get("note").is_some(), "nullable null kept");
        assert_eq!(validate(&cr, &policy_schema(), None), []);
    }

    #[test]
    fn value_validations_in_upstreams_words() {
        assert_eq!(errors(json!({"metadata": {"name": "a"}}), None), ["spec: Required value"]);
        assert_eq!(errors(json!({"spec": {}}), None), ["spec.selector: Required value"]);
        assert_eq!(errors(json!({"spec": {"selector": 7}}), None),
                   ["spec.selector: Invalid value: \"integer\": spec.selector in body must be of type string: \"integer\""]);
        assert_eq!(errors(json!({"spec": {"selector": "Web"}}), None),
                   ["spec.selector: Invalid value: \"Web\": spec.selector in body should match '^[a-z]+$'"]);
        assert_eq!(errors(json!({"spec": {"selector": "abcdefghij"}}), None),
                   ["spec.selector: Too long: may not be more than 8 bytes"]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "family": "IPX"}}), None),
                   ["spec.family: Unsupported value: \"IPX\": supported values: \"IPv4\", \"IPv6\""]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "priority": 11}}), None),
                   ["spec.priority: Invalid value: 11: spec.priority in body should be less than or equal to 10"]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "priority": 0}}), None),
                   ["spec.priority: Invalid value: 0: spec.priority in body should be greater than or equal to 1"]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "weight": 0}}), None),
                   ["spec.weight: Invalid value: 0: spec.weight in body should be greater than 0"]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "weight": 0.7}}), None),
                   ["spec.weight: Invalid value: 0.7: spec.weight in body should be a multiple of 0.5"]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "port": true}}), None),
                   ["spec.port: Invalid value: \"boolean\": spec.port in body must be of type integer,string: \"boolean\""]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "labels": {"a": "1", "b": "2", "c": "3"}}}), None),
                   ["spec.labels: Too many: 3: must have at most 2 items"]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "labels": {"a": 1}}}), None),
                   ["spec.labels.a: Invalid value: \"integer\": spec.labels.a in body must be of type string: \"integer\""]);
    }

    /// C6: a keyed list may not repeat a key (the defaulted protocol counts),
    /// a set may not repeat a value.
    #[test]
    fn keyed_lists_and_sets_refuse_repeats() {
        assert_eq!(errors(json!({"spec": {"selector": "a", "rules": [{"name": "a"}, {"name": "a", "protocol": "TCP"}]}}), None),
                   ["spec.rules[1]: Duplicate value: map[string]interface {}{\"name\":\"a\", \"protocol\":\"TCP\"}"]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "tags": ["x", "x"]}}), None),
                   ["spec.tags[1]: Duplicate value: \"x\""]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "rules": [{"name": "a"}, {"name": "b"}, {"name": "c"}, {"name": "d"}]}}), None),
                   ["spec.rules: Too many: 4: must have at most 3 items"]);
        assert_eq!(errors(json!({"spec": {"selector": "a", "rules": [{"protocol": "UDP"}]}}), None),
                   ["spec.rules[0].name: Required value"]);
    }

    /// A null the schema does not allow is pruned (then defaulted), not an
    /// error, as upstream does; in a list it is a type error.
    #[test]
    fn nulls_are_pruned_where_not_nullable() {
        let mut cr = json!({"spec": {"selector": "a", "priority": null, "family": null, "tags": [null]}});
        apply(&mut cr, &policy_schema());
        assert!(cr["spec"].get("priority").is_none());
        assert_eq!(cr["spec"]["family"], "IPv4");
        assert_eq!(validate(&cr, &policy_schema(), None).iter().map(ToString::to_string).collect::<Vec<_>>(),
                   ["spec.tags[0]: Invalid value: \"null\": spec.tags[0] in body must be of type string: \"null\""]);
    }

    /// Ratcheting: an update may keep a value its stored object already had,
    /// however the schema has changed since; a changed or new one is held to
    /// the schema.
    #[test]
    fn updates_ratchet_unchanged_values() {
        let stored = json!({"spec": {"selector": "Old", "priority": 50}});
        assert_eq!(errors(json!({"spec": {"selector": "Old", "priority": 50, "tags": ["t"]}}), Some(&stored)), Vec::<String>::new());
        assert_eq!(errors(json!({"spec": {"selector": "Old", "priority": 60}}), Some(&stored)),
                   ["spec.priority: Invalid value: 60: spec.priority in body should be less than or equal to 10"]);
        // Missing before and now: ratcheted. Present before, removed: refused.
        assert_eq!(errors(json!({"metadata": {}}), Some(&json!({"metadata": {}}))), Vec::<String>::new());
        assert_eq!(errors(json!({"spec": {}}), Some(&json!({"spec": {"selector": "a"}}))), ["spec.selector: Required value"]);
    }

    #[test]
    fn the_invalid_message_names_the_object() {
        let cr = json!({"apiVersion": "net.io/v1", "kind": "Policy", "metadata": {"name": "p"}});
        let one = validate(&json!({"metadata": {}}), &policy_schema(), None);
        assert_eq!(invalid_message(&cr, &one), "Policy.net.io \"p\" is invalid: spec: Required value");
        let two = validate(&json!({"spec": {"selector": 1, "priority": 0}}), &policy_schema(), None);
        assert_eq!(invalid_message(&cr, &two), "Policy.net.io \"p\" is invalid: [spec.priority: Invalid value: 0: \
            spec.priority in body should be greater than or equal to 1, spec.selector: Invalid value: \"integer\": \
            spec.selector in body must be of type string: \"integer\"]");
    }

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
