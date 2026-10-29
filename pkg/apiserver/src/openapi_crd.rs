//! Custom resources in `/openapi/v2` and `/openapi/v3` (#120).
//!
//! Each served version of each registered CRD is published from its
//! `openAPIV3Schema`, as upstream's CRD OpenAPI builder publishes it:
//!
//! - the definition is named `{reversed group}.{version}.{Kind}`
//!   (`com.example.stable.v1.CronTab`), as `kubectl explain` and the
//!   conformance suite look it up;
//! - it is the schema with `apiVersion`, `kind` and `metadata` added as
//!   properties (upstream's descriptions) and
//!   `x-kubernetes-group-version-kind`;
//! - v2 drops what Swagger 2.0 cannot say (`nullable`, `oneOf`, `anyOf`,
//!   `not`, and the `type` of an `x-kubernetes-int-or-string` field); v3
//!   keeps the schema whole;
//! - a CRD without a schema is published as an object that keeps unknown
//!   fields.
//!
//! Built from the CRD registry on every request, so a CRD that is created,
//! changed (a version renamed or no longer served) or deleted is reflected
//! at once. `metadata` is ObjectMeta inline rather than a `$ref` (the
//! built-in types are not published here, #31): its fields and upstream's
//! descriptions, read from the vendored `meta/v1` proto, so `kubectl explain
//! <cr>.metadata` lists them as upstream's does.

use crate::crd::{CrdDefinition, CrdScope};
use serde_json::{json, Map, Value};

const API_VERSION_DOC: &str = "APIVersion defines the versioned schema of this representation of an object. \
Servers should convert recognized schemas to the latest internal value, and may reject unrecognized values. \
More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#resources";
const KIND_DOC: &str = "Kind is a string value representing the REST resource this object represents. \
Servers may infer this from the endpoint the client submits requests to. Cannot be updated. In CamelCase. \
More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#types-kinds";
const METADATA_DOC: &str = "Standard object's metadata. \
More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata";

/// `crd-publish.example.com` → `com.example.crd-publish`.
fn reversed(group: &str) -> String {
    group.split('.').rev().collect::<Vec<_>>().join(".")
}

/// The published name of a CRD version's definition.
pub fn definition_name(def: &CrdDefinition) -> String {
    format!("{}.{}.{}", reversed(&def.group), def.version, def.kind)
}

/// Swagger 2.0 cannot say these; upstream's v2 conversion drops them.
fn to_v2(schema: &mut Value) {
    match schema {
        Value::Object(m) => {
            for k in ["nullable", "oneOf", "anyOf", "not"] {
                m.remove(k);
            }
            if m.get("x-kubernetes-int-or-string") == Some(&Value::Bool(true)) {
                m.remove("type");
            }
            for v in m.values_mut() {
                to_v2(v);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(to_v2),
        _ => {}
    }
}

/// The boolean extensions upstream writes only when they are true. A CRD
/// created over protobuf comes back with every one of them `false` on every
/// node, and the conformance suite's comparison saw a different schema
/// (#120: seven CustomResourcePublishOpenAPI specs timed out on it).
const TRUE_ONLY: [&str; 3] =
    ["x-kubernetes-embedded-resource", "x-kubernetes-int-or-string", "x-kubernetes-preserve-unknown-fields"];

fn drop_false_flags(schema: &mut Value) {
    match schema {
        Value::Object(m) => {
            for k in TRUE_ONLY {
                if m.get(k) == Some(&Value::Bool(false)) {
                    m.remove(k);
                }
            }
            m.values_mut().for_each(drop_false_flags);
        }
        Value::Array(a) => a.iter_mut().for_each(drop_false_flags),
        _ => {}
    }
}

/// ObjectMeta's properties, from the vendored proto's fields and doc comments
/// (upstream's OpenAPI descriptions are those comments).
fn object_meta_properties() -> &'static Map<String, Value> {
    static PROPS: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    PROPS.get_or_init(|| {
        const PROTO: &str = include_str!("../../apimachinery/protos/k8s.io/apimachinery/pkg/apis/meta/v1/generated.proto");
        let mut props = Map::new();
        let Some(start) = PROTO.find("message ObjectMeta {") else { return props };
        let body = &PROTO[start..];
        let body = &body[..body.find("\n}").unwrap_or(body.len())];
        let mut doc: Vec<String> = Vec::new();
        for line in body.lines().skip(1).map(str::trim) {
            if let Some(c) = line.strip_prefix("//") {
                let c = c.trim();
                if !c.starts_with('+') {
                    doc.push(c.to_string());
                }
                continue;
            }
            let words: Vec<&str> = line.split_whitespace().collect();
            let (ty, name) = match words.as_slice() {
                ["optional", ty, name, ..] => (*ty, *name),
                ["repeated", ty, name, ..] => (*ty, *name),
                [m, _, name, ..] if m.starts_with("map<") => ("map", *name),
                _ => {
                    doc.clear();
                    continue;
                }
            };
            let scalar = |t: &str| match t {
                "string" => json!({"type": "string"}),
                "int64" | "int32" => json!({"type": "integer", "format": t}),
                "Time" => json!({"type": "string", "format": "date-time"}),
                _ => json!({"type": "object"}),
            };
            let mut field = match (words[0], ty) {
                (_, "map") => json!({"type": "object", "additionalProperties": {"type": "string"}}),
                ("repeated", t) => json!({"type": "array", "items": scalar(t)}),
                (_, t) => scalar(t),
            };
            // Comment lines joined; an empty one is a paragraph break.
            let text = doc.join("\n").split("\n\n").map(|p| p.replace('\n', " ")).collect::<Vec<_>>().join("\n\n");
            field["description"] = json!(text.trim());
            props.insert(name.to_string(), field);
            doc.clear();
        }
        props
    })
}

/// The published schema of one CRD version.
pub fn definition(def: &CrdDefinition, v2: bool) -> Value {
    let mut s = match &def.schema {
        Some(schema) => (**schema).clone(),
        // v2 says only "an object", as upstream; v3 keeps unknown fields.
        None if v2 => json!({"type": "object"}),
        None => json!({"type": "object", "x-kubernetes-preserve-unknown-fields": true}),
    };
    drop_false_flags(&mut s);
    if v2 {
        to_v2(&mut s);
    }
    if !s.is_object() {
        s = json!({"type": "object"});
    }
    let props = s.as_object_mut().unwrap().entry("properties").or_insert_with(|| json!({}));
    if let Some(p) = props.as_object_mut() {
        p.insert("apiVersion".into(), json!({"type": "string", "description": API_VERSION_DOC}));
        p.insert("kind".into(), json!({"type": "string", "description": KIND_DOC}));
        p.insert("metadata".into(), json!({"type": "object", "description": METADATA_DOC,
                                           "properties": object_meta_properties()}));
    }
    s["x-kubernetes-group-version-kind"] = json!([{"group": def.group, "version": def.version, "kind": def.kind}]);
    s
}

/// `definitions` for `/openapi/v2`.
pub fn v2_definitions(defs: &[CrdDefinition]) -> Map<String, Value> {
    defs.iter().map(|d| (definition_name(d), definition(d, true))).collect()
}

/// The `/openapi/v3` index entries for CRD group-versions.
pub fn v3_group_versions(defs: &[CrdDefinition]) -> Vec<String> {
    let mut gvs: Vec<String> = defs.iter().map(|d| format!("apis/{}/{}", d.group, d.version)).collect();
    gvs.dedup();
    gvs
}

/// Paths and `components.schemas` of the CRDs in one group-version.
pub fn v3_parts(defs: &[CrdDefinition], group: &str, version: &str) -> (Map<String, Value>, Map<String, Value>) {
    let mut paths = Map::new();
    let mut schemas = Map::new();
    for d in defs.iter().filter(|d| d.group == group && d.version == version) {
        let name = definition_name(d);
        let gvk = json!({"group": d.group, "version": d.version, "kind": d.kind});
        let prefix = format!("/apis/{group}/{version}");
        let (collection, item) = match d.scope {
            CrdScope::Namespaced => (
                format!("{prefix}/namespaces/{{namespace}}/{}", d.plural),
                format!("{prefix}/namespaces/{{namespace}}/{}/{{name}}", d.plural),
            ),
            CrdScope::Cluster => (format!("{prefix}/{}", d.plural), format!("{prefix}/{}/{{name}}", d.plural)),
        };
        let reference = json!({"$ref": format!("#/components/schemas/{name}")});
        let op = |body: bool| {
            let mut o = crate::discovery::operation(&gvk);
            o["responses"]["200"]["content"] = json!({"application/json": {"schema": reference}});
            if body {
                o["requestBody"] = json!({"content": {"application/json": {"schema": reference}}});
            }
            o
        };
        paths.insert(collection, json!({"post": op(true)}));
        paths.insert(item, json!({"get": op(false), "put": op(true), "patch": op(false)}));
        schemas.insert(name, definition(d, false));
    }
    (paths, schemas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn def(schema: Option<Value>) -> CrdDefinition {
        CrdDefinition {
            group: "crd-publish-openapi-test-foo.example.com".into(), version: "v1".into(), kind: "E2eTestFoo".into(),
            plural: "e2e-test-foos".into(), singular: "e2e-test-foo".into(), short_names: vec![],
            scope: CrdScope::Namespaced, printer_columns: vec![], status_subresource: false,
            schema: schema.map(Arc::new), scale: None,
        }
    }

    /// The conformance check: the definition, less the three properties and
    /// the GVK extension it adds, is the CRD's schema.
    #[test]
    fn the_definition_is_the_schema_plus_what_upstream_adds() {
        let schema = json!({"description": "Foo CRD for Testing", "type": "object", "properties": {
            "spec": {"type": "object", "description": "Specification of Foo", "properties": {
                "bars": {"description": "List of Bars and their specs.", "type": "array", "items": {
                    "type": "object", "required": ["name"], "properties": {
                        "name": {"description": "Name of Bar.", "type": "string"},
                        "age": {"description": "Age of Bar.", "type": "string"}}}}}}}});
        let d = def(Some(schema.clone()));
        assert_eq!(definition_name(&d), "com.example.crd-publish-openapi-test-foo.v1.E2eTestFoo");
        let mut got = v2_definitions(&[d])["com.example.crd-publish-openapi-test-foo.v1.E2eTestFoo"].clone();
        assert_eq!(got["x-kubernetes-group-version-kind"][0]["kind"], "E2eTestFoo");
        assert!(got["properties"]["apiVersion"]["description"].as_str().unwrap().starts_with("APIVersion defines"));
        for p in ["apiVersion", "kind", "metadata"] {
            got["properties"].as_object_mut().unwrap().remove(p);
        }
        got.as_object_mut().unwrap().remove("x-kubernetes-group-version-kind");
        assert_eq!(got, schema);
    }

    /// `kubectl explain <cr>.metadata` lists ObjectMeta's fields with
    /// upstream's descriptions (the conformance pattern: creationTimestamp
    /// <string>, "CreationTimestamp is a timestamp").
    #[test]
    fn metadata_is_object_meta_with_its_descriptions() {
        let got = definition(&def(None), false);
        let meta = &got["properties"]["metadata"]["properties"];
        assert_eq!(meta["creationTimestamp"]["type"], "string");
        assert!(meta["creationTimestamp"]["description"].as_str().unwrap().starts_with("CreationTimestamp is a timestamp"));
        assert_eq!(meta["labels"]["additionalProperties"]["type"], "string");
        assert_eq!(meta["ownerReferences"]["type"], "array");
        assert_eq!(meta["generation"]["type"], "integer");
        assert!(!meta["name"]["description"].as_str().unwrap().contains("+optional"));
        assert_eq!(meta.as_object().unwrap().len(), 15, "{:?}", meta.as_object().unwrap().keys().collect::<Vec<_>>());
    }

    #[test]
    fn false_extension_flags_are_not_published() {
        let schema = json!({"type": "object", "x-kubernetes-embedded-resource": false,
            "x-kubernetes-int-or-string": false, "x-kubernetes-preserve-unknown-fields": false,
            "properties": {"spec": {"type": "object", "x-kubernetes-embedded-resource": false,
                "x-kubernetes-preserve-unknown-fields": true}}});
        for v2 in [true, false] {
            let got = definition(&def(Some(schema.clone())), v2);
            for k in TRUE_ONLY {
                assert!(got.get(k).is_none(), "{k} false published (v2 {v2})");
            }
            assert!(got["properties"]["spec"].get("x-kubernetes-embedded-resource").is_none());
            assert_eq!(got["properties"]["spec"]["x-kubernetes-preserve-unknown-fields"], true, "true kept");
        }
    }

    #[test]
    fn v2_drops_what_swagger_cannot_say_and_v3_keeps_it() {
        let d = def(Some(json!({"type": "object", "properties": {
            "port": {"x-kubernetes-int-or-string": true, "anyOf": [{"type": "integer"}, {"type": "string"}]},
            "note": {"type": "string", "nullable": true}}})));
        let v2 = definition(&d, true);
        assert!(v2["properties"]["port"].get("anyOf").is_none() && v2["properties"]["note"].get("nullable").is_none());
        let v3 = definition(&d, false);
        assert!(v3["properties"]["port"]["anyOf"].is_array() && v3["properties"]["note"]["nullable"] == true);
        let (paths, schemas) = v3_parts(&[d], "crd-publish-openapi-test-foo.example.com", "v1");
        assert!(schemas.contains_key("com.example.crd-publish-openapi-test-foo.v1.E2eTestFoo"));
        assert!(paths.contains_key("/apis/crd-publish-openapi-test-foo.example.com/v1/namespaces/{namespace}/e2e-test-foos"));
        assert!(def(None).schema.is_none() && definition(&def(None), false)["x-kubernetes-preserve-unknown-fields"] == true);
        assert!(definition(&def(None), true).get("x-kubernetes-preserve-unknown-fields").is_none(), "v2: an object, as upstream");
    }
}
