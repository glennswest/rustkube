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
//! at once. `metadata` is an inline object, not a `$ref` to ObjectMeta: the
//! built-in types are not published here (their schemas are the server's,
//! #31).

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

/// The published schema of one CRD version.
pub fn definition(def: &CrdDefinition, v2: bool) -> Value {
    let mut s = match &def.schema {
        Some(schema) => (**schema).clone(),
        None => json!({"type": "object", "x-kubernetes-preserve-unknown-fields": true}),
    };
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
        p.insert("metadata".into(), json!({"type": "object", "description": METADATA_DOC}));
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
        assert!(def(None).schema.is_none() && definition(&def(None), true)["x-kubernetes-preserve-unknown-fields"] == true);
    }
}
