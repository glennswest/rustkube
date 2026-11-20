//! The protobuf encoding of the `/openapi/v2` document (#343).
//!
//! client-go's `OpenAPISchema()` (behind `kubectl explain -o
//! plaintext-openapiv2`, kubectl's v2 validation fallback and older clients)
//! asks for `application/com.github.proto-openapi.spec.v2@v1.0+protobuf` and
//! parses the body as a gnostic `openapi.v2.Document`. Upstream's apiserver
//! builds that message from the Swagger JSON with gnostic's compiler; this
//! module does the same for the parts of Swagger 2.0 the document rustkube
//! serves uses: `swagger`, `info`, `host`, `basePath`, `schemes`, `consumes`,
//! `produces`, `paths` (vendor extensions only — rustkube's v2 document has
//! no path items) and `definitions`, with every Schema keyword Swagger 2.0
//! defines except `xml`.
//!
//! As gnostic does, an `x-…` key becomes a `vendor_extension` entry and a
//! free-form value (`default`, `enum`, `example`, an extension) an
//! `openapi.v2.Any` carrying its YAML text — JSON, which is YAML; client-go
//! reads it back with a YAML parser. A key Swagger 2.0 has no field for is
//! not carried, as gnostic's typed message cannot carry it.
//!
//! The message is built from the vendored
//! `github.com/google/gnostic-models/openapiv2/OpenAPIv2.proto` (v0.7.0, the
//! version Kubernetes 1.36 pins), so field numbers come from upstream's file.

use prost::Message as _;
use prost_reflect::{DynamicMessage, MessageDescriptor, ReflectMessage, Value as PbValue};
use serde_json::{Map, Value};

/// The media type client-go sends (and upstream answers with).
pub const CONTENT_TYPE: &str = "application/com.github.proto-openapi.spec.v2@v1.0+protobuf";
/// The older spelling upstream's handler also accepts.
pub const CONTENT_TYPE_LEGACY: &str = "application/com.github.proto-openapi.spec.v2.v1.0+protobuf";

/// What an `Accept` header asks `/openapi/v2` for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Protobuf,
}

/// Negotiate `/openapi/v2`'s encoding from an `Accept` header: the
/// acceptable type with the highest `q` (the first on a tie). No header, an
/// empty one or a wildcard is JSON. `None` when nothing offered is acceptable
/// — the caller answers 406, never JSON under a protobuf-only request.
pub fn negotiate(accept: Option<&str>) -> Option<Format> {
    let accept = match accept.map(str::trim) {
        None | Some("") => return Some(Format::Json),
        Some(a) => a,
    };
    let mut best: Option<(f32, Format)> = None;
    for part in accept.split(',') {
        let mut params = part.split(';');
        let mt = params.next().unwrap_or("").trim().to_ascii_lowercase();
        let mut q = 1.0f32;
        for p in params {
            if let Some((k, v)) = p.split_once('=') {
                if k.trim().eq_ignore_ascii_case("q") {
                    q = v.trim().parse().unwrap_or(0.0);
                }
            }
        }
        if q <= 0.0 {
            continue;
        }
        let format = match mt.as_str() {
            CONTENT_TYPE | CONTENT_TYPE_LEGACY => Format::Protobuf,
            "application/json" | "application/*" | "*/*" => Format::Json,
            _ => continue,
        };
        if !matches!(best, Some((bq, _)) if q <= bq) {
            best = Some((q, format));
        }
    }
    best.map(|(_, f)| f)
}

fn desc(name: &str) -> Result<MessageDescriptor, String> {
    crate::protobuf::pool()
        .get_message_by_name(&format!("openapi.v2.{name}"))
        .ok_or_else(|| format!("openapi.v2.{name} is not in the descriptor set"))
}

fn set(msg: &mut DynamicMessage, field: &str, value: PbValue) -> Result<(), String> {
    let f = msg
        .descriptor()
        .get_field_by_name(field)
        .ok_or_else(|| format!("{} has no field {field}", msg.descriptor().full_name()))?;
    msg.try_set_field(&f, value).map_err(|e| format!("{}.{field}: {e}", msg.descriptor().full_name()))
}

fn string(v: &Value, what: &str) -> Result<PbValue, String> {
    v.as_str().map(|s| PbValue::String(s.to_string())).ok_or_else(|| format!("{what} is not a string"))
}

fn strings(v: &Value, what: &str) -> Result<PbValue, String> {
    let items = v.as_array().ok_or_else(|| format!("{what} is not a list"))?;
    Ok(PbValue::List(items.iter().map(|i| string(i, what)).collect::<Result<_, _>>()?))
}

fn object<'a>(v: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
    v.as_object().ok_or_else(|| format!("{what} is not an object"))
}

/// An `openapi.v2.Any`: the value as YAML text (JSON is YAML).
fn any(v: &Value) -> Result<PbValue, String> {
    let mut m = DynamicMessage::new(desc("Any")?);
    set(&mut m, "yaml", PbValue::String(v.to_string()))?;
    Ok(PbValue::Message(m))
}

/// The `x-…` keys of an object as `repeated NamedAny`.
fn vendor_extensions(obj: &Map<String, Value>) -> Result<Vec<PbValue>, String> {
    let named = desc("NamedAny")?;
    let mut out = Vec::new();
    for (k, v) in obj.iter().filter(|(k, _)| k.starts_with("x-")) {
        let mut m = DynamicMessage::new(named.clone());
        set(&mut m, "name", PbValue::String(k.clone()))?;
        set(&mut m, "value", any(v)?)?;
        out.push(PbValue::Message(m));
    }
    Ok(out)
}

fn set_extensions(msg: &mut DynamicMessage, obj: &Map<String, Value>) -> Result<(), String> {
    let ext = vendor_extensions(obj)?;
    if !ext.is_empty() {
        set(msg, "vendor_extension", PbValue::List(ext))?;
    }
    Ok(())
}

/// A map of schemas as `repeated NamedSchema` in a wrapper message
/// (`Definitions`, `Properties`).
fn named_schemas(wrapper: &str, v: &Value, what: &str) -> Result<PbValue, String> {
    let named = desc("NamedSchema")?;
    let mut list = Vec::new();
    for (k, s) in object(v, what)? {
        let mut m = DynamicMessage::new(named.clone());
        set(&mut m, "name", PbValue::String(k.clone()))?;
        set(&mut m, "value", PbValue::Message(schema(s)?))?;
        list.push(PbValue::Message(m));
    }
    let mut w = DynamicMessage::new(desc(wrapper)?);
    set(&mut w, "additional_properties", PbValue::List(list))?;
    Ok(PbValue::Message(w))
}

fn f64_of(v: &Value, what: &str) -> Result<PbValue, String> {
    v.as_f64().map(PbValue::F64).ok_or_else(|| format!("{what} is not a number"))
}

fn i64_of(v: &Value, what: &str) -> Result<PbValue, String> {
    v.as_i64()
        .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
        .map(PbValue::I64)
        .ok_or_else(|| format!("{what} is not an integer"))
}

fn bool_of(v: &Value, what: &str) -> Result<PbValue, String> {
    v.as_bool().map(PbValue::Bool).ok_or_else(|| format!("{what} is not a boolean"))
}

/// A Swagger 2.0 Schema object as `openapi.v2.Schema`.
fn schema(v: &Value) -> Result<DynamicMessage, String> {
    let obj = object(v, "schema")?;
    let mut m = DynamicMessage::new(desc("Schema")?);
    for (k, val) in obj {
        match k.as_str() {
            "$ref" => set(&mut m, "_ref", string(val, k)?)?,
            "format" | "title" | "description" | "pattern" | "discriminator" => set(&mut m, k, string(val, k)?)?,
            "default" | "example" => set(&mut m, k, any(val)?)?,
            "multipleOf" => set(&mut m, "multiple_of", f64_of(val, k)?)?,
            "maximum" | "minimum" => set(&mut m, k, f64_of(val, k)?)?,
            "exclusiveMaximum" => set(&mut m, "exclusive_maximum", bool_of(val, k)?)?,
            "exclusiveMinimum" => set(&mut m, "exclusive_minimum", bool_of(val, k)?)?,
            "maxLength" => set(&mut m, "max_length", i64_of(val, k)?)?,
            "minLength" => set(&mut m, "min_length", i64_of(val, k)?)?,
            "maxItems" => set(&mut m, "max_items", i64_of(val, k)?)?,
            "minItems" => set(&mut m, "min_items", i64_of(val, k)?)?,
            "maxProperties" => set(&mut m, "max_properties", i64_of(val, k)?)?,
            "minProperties" => set(&mut m, "min_properties", i64_of(val, k)?)?,
            "uniqueItems" => set(&mut m, "unique_items", bool_of(val, k)?)?,
            "readOnly" => set(&mut m, "read_only", bool_of(val, k)?)?,
            "required" => set(&mut m, "required", strings(val, k)?)?,
            "enum" => {
                let items = val.as_array().ok_or("enum is not a list")?;
                set(&mut m, "enum", PbValue::List(items.iter().map(any).collect::<Result<_, _>>()?))?
            }
            "type" => {
                // A single type or a list of them, as gnostic's TypeItem.
                let list = match val {
                    Value::String(_) => PbValue::List(vec![string(val, k)?]),
                    _ => strings(val, k)?,
                };
                let mut t = DynamicMessage::new(desc("TypeItem")?);
                set(&mut t, "value", list)?;
                set(&mut m, "type", PbValue::Message(t))?
            }
            "items" => {
                let schemas = match val {
                    Value::Array(a) => a.iter().map(|s| schema(s).map(PbValue::Message)).collect::<Result<_, _>>()?,
                    _ => vec![PbValue::Message(schema(val)?)],
                };
                let mut it = DynamicMessage::new(desc("ItemsItem")?);
                set(&mut it, "schema", PbValue::List(schemas))?;
                set(&mut m, "items", PbValue::Message(it))?
            }
            "allOf" => {
                let items = val.as_array().ok_or("allOf is not a list")?;
                let list = items.iter().map(|s| schema(s).map(PbValue::Message)).collect::<Result<_, _>>()?;
                set(&mut m, "all_of", PbValue::List(list))?
            }
            "properties" => set(&mut m, "properties", named_schemas("Properties", val, k)?)?,
            "additionalProperties" => {
                let mut ap = DynamicMessage::new(desc("AdditionalPropertiesItem")?);
                match val {
                    Value::Bool(b) => set(&mut ap, "boolean", PbValue::Bool(*b))?,
                    _ => set(&mut ap, "schema", PbValue::Message(schema(val)?))?,
                }
                set(&mut m, "additional_properties", PbValue::Message(ap))?
            }
            "externalDocs" => set(&mut m, "external_docs", external_docs(val)?)?,
            // `x-…` below; anything else has no field in Swagger 2.0's Schema.
            _ => {}
        }
    }
    set_extensions(&mut m, obj)?;
    Ok(m)
}

fn external_docs(v: &Value) -> Result<PbValue, String> {
    let obj = object(v, "externalDocs")?;
    let mut m = DynamicMessage::new(desc("ExternalDocs")?);
    for k in ["description", "url"] {
        if let Some(s) = obj.get(k) {
            set(&mut m, k, string(s, k)?)?;
        }
    }
    set_extensions(&mut m, obj)?;
    Ok(PbValue::Message(m))
}

/// Encode a Swagger 2.0 JSON document as a gnostic `openapi.v2.Document`.
///
/// A document using a part of Swagger 2.0 this encoder does not carry (a path
/// item, `parameters`, `responses`, security, tags) is an error rather than a
/// message that silently lacks it.
pub fn encode_document(doc: &Value) -> Result<Vec<u8>, String> {
    let obj = object(doc, "document")?;
    let mut m = DynamicMessage::new(desc("Document")?);
    for (k, val) in obj {
        match k.as_str() {
            "swagger" | "host" => set(&mut m, k, string(val, k)?)?,
            "basePath" => set(&mut m, "base_path", string(val, k)?)?,
            "schemes" | "consumes" | "produces" => set(&mut m, k, strings(val, k)?)?,
            "info" => {
                let io = object(val, k)?;
                let mut info = DynamicMessage::new(desc("Info")?);
                for f in ["title", "version", "description"] {
                    if let Some(s) = io.get(f) {
                        set(&mut info, f, string(s, f)?)?;
                    }
                }
                if let Some(s) = io.get("termsOfService") {
                    set(&mut info, "terms_of_service", string(s, "termsOfService")?)?;
                }
                set_extensions(&mut info, io)?;
                set(&mut m, "info", PbValue::Message(info))?
            }
            "paths" => {
                let po = object(val, k)?;
                if let Some(p) = po.keys().find(|p| !p.starts_with("x-")) {
                    return Err(format!("path item {p} cannot be encoded"));
                }
                let mut paths = DynamicMessage::new(desc("Paths")?);
                set_extensions(&mut paths, po)?;
                set(&mut m, "paths", PbValue::Message(paths))?
            }
            "definitions" => set(&mut m, "definitions", named_schemas("Definitions", val, k)?)?,
            "externalDocs" => set(&mut m, "external_docs", external_docs(val)?)?,
            x if x.starts_with("x-") => {}
            other => return Err(format!("document key {other} cannot be encoded")),
        }
    }
    set_extensions(&mut m, obj)?;
    Ok(m.encode_to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn negotiation_follows_accept() {
        assert_eq!(negotiate(None), Some(Format::Json));
        assert_eq!(negotiate(Some("")), Some(Format::Json));
        assert_eq!(negotiate(Some("*/*")), Some(Format::Json));
        assert_eq!(negotiate(Some("application/json")), Some(Format::Json));
        // What client-go's OpenAPISchema() sends, and only that.
        assert_eq!(negotiate(Some(CONTENT_TYPE)), Some(Format::Protobuf));
        assert_eq!(negotiate(Some(CONTENT_TYPE_LEGACY)), Some(Format::Protobuf));
        assert_eq!(negotiate(Some(&format!("{CONTENT_TYPE}, application/json"))), Some(Format::Protobuf));
        assert_eq!(negotiate(Some(&format!("application/json, {CONTENT_TYPE}"))), Some(Format::Json));
        assert_eq!(negotiate(Some(&format!("application/json;q=0.5, {CONTENT_TYPE}"))), Some(Format::Protobuf));
        assert_eq!(negotiate(Some(&format!("{CONTENT_TYPE};q=0, application/json"))), Some(Format::Json));
        // Nothing acceptable: 406, not JSON.
        assert_eq!(negotiate(Some("application/vnd.kubernetes.protobuf")), None);
        assert_eq!(negotiate(Some("application/json;q=0")), None);
    }

    fn named<'a>(list: &'a prost_reflect::Value, name: &str) -> &'a DynamicMessage {
        list.as_list()
            .unwrap()
            .iter()
            .map(|v| v.as_message().unwrap())
            .find(|m| m.get_field_by_name("name").unwrap().as_str() == Some(name))
            .unwrap_or_else(|| panic!("no entry {name}"))
    }

    #[test]
    fn document_round_trips_through_gnostic_message() {
        let doc = json!({
            "swagger": "2.0",
            "info": {"title": "Kubernetes", "version": "v1.36.0"},
            "paths": {},
            "definitions": {
                "io.kubevirt.v1.VirtualMachine": {
                    "type": "object",
                    "description": "A VM",
                    "required": ["spec"],
                    "properties": {
                        "spec": {
                            "type": "object",
                            "properties": {
                                "running": {"type": "boolean", "default": false},
                                "runStrategy": {"type": "string", "enum": ["Always", "Halted"]},
                                "replicas": {"type": "integer", "format": "int32", "minimum": 0, "maxLength": 3},
                                "disks": {"type": "array", "items": {"$ref": "#/definitions/Disk"}},
                                "labels": {"type": "object", "additionalProperties": {"type": "string"}},
                                "port": {"x-kubernetes-int-or-string": true}
                            }
                        }
                    },
                    "x-kubernetes-group-version-kind": [{"group": "kubevirt.io", "kind": "VirtualMachine", "version": "v1"}]
                }
            }
        });
        let bytes = encode_document(&doc).unwrap();
        let m = DynamicMessage::decode(desc("Document").unwrap(), bytes.as_slice()).unwrap();
        assert_eq!(m.get_field_by_name("swagger").unwrap().as_str(), Some("2.0"));
        let info = m.get_field_by_name("info").unwrap();
        assert_eq!(info.as_message().unwrap().get_field_by_name("title").unwrap().as_str(), Some("Kubernetes"));
        assert!(m.has_field_by_name("paths"));

        let defs = m.get_field_by_name("definitions").unwrap();
        let defs = defs.as_message().unwrap().get_field_by_name("additional_properties").unwrap();
        let vm = named(&defs, "io.kubevirt.v1.VirtualMachine");
        let vm = vm.get_field_by_name("value").unwrap();
        let vm = vm.as_message().unwrap();
        assert_eq!(vm.get_field_by_name("description").unwrap().as_str(), Some("A VM"));
        let ty = vm.get_field_by_name("type").unwrap();
        let ty = ty.as_message().unwrap().get_field_by_name("value").unwrap();
        assert_eq!(ty.as_list().unwrap()[0].as_str(), Some("object"));
        assert_eq!(vm.get_field_by_name("required").unwrap().as_list().unwrap()[0].as_str(), Some("spec"));

        // The GVK extension, as kube-openapi's VendorExtensionToMap reads it.
        let ext = vm.get_field_by_name("vendor_extension").unwrap();
        let gvk = named(&ext, "x-kubernetes-group-version-kind").get_field_by_name("value").unwrap();
        let yaml = gvk.as_message().unwrap().get_field_by_name("yaml").unwrap();
        let back: Value = serde_json::from_str(yaml.as_str().unwrap()).unwrap();
        assert_eq!(back[0]["kind"], "VirtualMachine");

        // Nested properties, items $ref, additionalProperties, enum, numbers.
        let props = vm.get_field_by_name("properties").unwrap();
        let props = props.as_message().unwrap().get_field_by_name("additional_properties").unwrap();
        let spec = named(&props, "spec").get_field_by_name("value").unwrap();
        let sp = spec.as_message().unwrap().get_field_by_name("properties").unwrap();
        let sp = sp.as_message().unwrap().get_field_by_name("additional_properties").unwrap();
        let field = |n: &str| named(&sp, n).get_field_by_name("value").unwrap().as_message().unwrap().clone();

        let replicas = field("replicas");
        assert_eq!(replicas.get_field_by_name("format").unwrap().as_str(), Some("int32"));
        assert_eq!(replicas.get_field_by_name("minimum").unwrap().as_f64(), Some(0.0));
        assert_eq!(replicas.get_field_by_name("max_length").unwrap().as_i64(), Some(3));

        let disks = field("disks");
        let items = disks.get_field_by_name("items").unwrap();
        let items = items.as_message().unwrap().get_field_by_name("schema").unwrap();
        let first = items.as_list().unwrap()[0].as_message().unwrap().clone();
        assert_eq!(first.get_field_by_name("_ref").unwrap().as_str(), Some("#/definitions/Disk"));

        let labels = field("labels");
        let ap = labels.get_field_by_name("additional_properties").unwrap();
        assert!(ap.as_message().unwrap().has_field_by_name("schema"));

        let rs = field("runStrategy");
        let en = rs.get_field_by_name("enum").unwrap();
        let first = en.as_list().unwrap()[0].as_message().unwrap().get_field_by_name("yaml").unwrap();
        assert_eq!(first.as_str(), Some("\"Always\""));

        let running = field("running");
        let d = running.get_field_by_name("default").unwrap();
        assert_eq!(d.as_message().unwrap().get_field_by_name("yaml").unwrap().as_str(), Some("false"));
    }

    #[test]
    fn unencodable_parts_are_errors_not_omissions() {
        assert!(encode_document(&json!({"swagger": "2.0", "paths": {"/api/v1/pods": {}}})).is_err());
        assert!(encode_document(&json!({"swagger": "2.0", "parameters": {}})).is_err());
    }
}
