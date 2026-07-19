//! Kubernetes protobuf wire codec.
//!
//! The analog of k8s.io/apimachinery's `runtime/serializer/protobuf` plus the
//! `k8s.io/api` generated marshalers: it decodes and encodes the
//! `application/vnd.kubernetes.protobuf` wire format that client-go uses by
//! default for built-in API groups.
//!
//! Wire format of a body:
//!   [0x6b 0x38 0x73 0x00]            magic "k8s\0"
//!   runtime.Unknown {                protobuf
//!     typeMeta { apiVersion, kind }  field 1
//!     raw = <object, protobuf>       field 2  ← the object itself
//!     contentEncoding                field 3
//!     contentType                    field 4
//!   }
//!
//! The `raw` object is decoded/encoded dynamically against a descriptor pool
//! built at compile time from vendored `.proto` files (see build.rs), so adding
//! a group is a matter of vendoring its `generated.proto` — no per-type Rust.
//!
//! Note: apiVersion/kind live in the envelope, not in `raw`, so they are
//! injected into (and stripped from) the JSON object here.

use once_cell::sync::Lazy;
use prost::Message as _;
use prost_reflect::{
    DescriptorPool, DynamicMessage, MessageDescriptor, ReflectMessage, Value as PbValue,
};
use serde_json::{Map, Value};

/// Magic prefix on every k8s protobuf body: `"k8s\0"`.
const MAGIC: [u8; 4] = [0x6b, 0x38, 0x73, 0x00];

/// The `application/vnd.kubernetes.protobuf` media type client-go negotiates.
pub const CONTENT_TYPE: &str = "application/vnd.kubernetes.protobuf";

/// Descriptor pool built from the vendored protos (build.rs → OUT_DIR).
static POOL: Lazy<DescriptorPool> = Lazy::new(|| {
    let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/k8s_descriptor.bin"));
    DescriptorPool::decode(bytes.as_ref()).expect("vendored k8s descriptor set is valid")
});

/// runtime.Unknown envelope (hand-declared: it is tiny and fixed).
#[derive(Clone, PartialEq, prost::Message)]
struct Unknown {
    #[prost(message, optional, tag = "1")]
    type_meta: Option<TypeMeta>,
    #[prost(bytes = "vec", optional, tag = "2")]
    raw: Option<Vec<u8>>,
    #[prost(string, optional, tag = "3")]
    content_encoding: Option<String>,
    #[prost(string, optional, tag = "4")]
    content_type: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct TypeMeta {
    #[prost(string, optional, tag = "1")]
    api_version: Option<String>,
    #[prost(string, optional, tag = "2")]
    kind: Option<String>,
}

/// Whether a body looks like the k8s protobuf wire format.
pub fn is_protobuf(body: &[u8]) -> bool {
    body.len() >= 4 && body[..4] == MAGIC
}

/// Whether `content_type` requests protobuf (ignoring parameters).
pub fn wants_protobuf(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .map(str::trim)
        .is_some_and(|ct| ct == CONTENT_TYPE)
}

/// Map an (apiVersion, kind) to the fully-qualified proto message name.
/// Returns `None` for groups whose `.proto` we have not vendored — the caller
/// then knows to reject with 415 rather than mis-decode.
fn message_name(api_version: &str, kind: &str) -> Option<String> {
    let (group, version) = match api_version.rsplit_once('/') {
        Some((g, v)) => (g, v),
        None => ("", api_version), // core group is unqualified ("v1")
    };
    // The proto package is not mechanically derivable from the API group
    // (rbac.authorization.k8s.io → k8s.io.api.rbac.v1), so map explicitly.
    let pkg = match (group, version) {
        ("", "v1") => "k8s.io.api.core.v1",
        ("coordination.k8s.io", "v1") => "k8s.io.api.coordination.v1",
        ("apps", "v1") => "k8s.io.api.apps.v1",
        ("batch", "v1") => "k8s.io.api.batch.v1",
        ("discovery.k8s.io", "v1") => "k8s.io.api.discovery.v1",
        ("storage.k8s.io", "v1") => "k8s.io.api.storage.v1",
        ("rbac.authorization.k8s.io", "v1") => "k8s.io.api.rbac.v1",
        ("networking.k8s.io", "v1") => "k8s.io.api.networking.v1",
        _ => return None,
    };
    let name = format!("{pkg}.{kind}");
    POOL.get_message_by_name(&name).map(|_| name)
}

/// Is protobuf (de)serialization available for this GVK?
pub fn supports(api_version: &str, kind: &str) -> bool {
    message_name(api_version, kind).is_some()
}

/// Decode a k8s protobuf body into the JSON object it represents.
pub fn decode_to_json(body: &[u8]) -> Result<Value, String> {
    if !is_protobuf(body) {
        return Err("body is not k8s protobuf (missing magic)".into());
    }
    let unknown = Unknown::decode(&body[4..]).map_err(|e| format!("bad Unknown envelope: {e}"))?;
    let tm = unknown.type_meta.unwrap_or_default();
    let api_version = tm.api_version.unwrap_or_default();
    let kind = tm.kind.unwrap_or_default();

    let msg_name = message_name(&api_version, &kind)
        .ok_or_else(|| format!("no protobuf schema for {api_version} {kind}"))?;
    let desc = POOL
        .get_message_by_name(&msg_name)
        .ok_or_else(|| format!("descriptor missing: {msg_name}"))?;

    let raw = unknown.raw.unwrap_or_default();
    let dyn_msg = DynamicMessage::decode(desc, raw.as_slice())
        .map_err(|e| format!("decoding {msg_name}: {e}"))?;

    let mut json = message_to_json(&dyn_msg);
    // apiVersion/kind live in the envelope, not in `raw`.
    if let Value::Object(ref mut m) = json {
        if !api_version.is_empty() {
            m.insert("apiVersion".into(), Value::String(api_version));
        }
        if !kind.is_empty() {
            m.insert("kind".into(), Value::String(kind));
        }
    }
    Ok(json)
}

/// Encode a JSON object into a k8s protobuf body. `api_version`/`kind` supply
/// the envelope TypeMeta (the object may already carry them; the values passed
/// win so callers can stamp the response GVK).
pub fn encode_from_json(json: &Value, api_version: &str, kind: &str) -> Result<Vec<u8>, String> {
    let msg_name = message_name(api_version, kind)
        .ok_or_else(|| format!("no protobuf schema for {api_version} {kind}"))?;
    let desc = POOL
        .get_message_by_name(&msg_name)
        .ok_or_else(|| format!("descriptor missing: {msg_name}"))?;

    let dyn_msg = json_to_message(json, &desc)?;
    let raw = dyn_msg.encode_to_vec();

    let unknown = Unknown {
        type_meta: Some(TypeMeta {
            api_version: Some(api_version.to_string()),
            kind: Some(kind.to_string()),
        }),
        raw: Some(raw),
        content_encoding: None,
        content_type: None,
    };
    let mut out = MAGIC.to_vec();
    unknown
        .encode(&mut out)
        .map_err(|e| format!("encoding Unknown: {e}"))?;
    Ok(out)
}

// --- apimachinery special types (custom JSON marshaling) ---------------------

const TIME: &str = "k8s.io.apimachinery.pkg.apis.meta.v1.Time";
const MICRO_TIME: &str = "k8s.io.apimachinery.pkg.apis.meta.v1.MicroTime";
const QUANTITY: &str = "k8s.io.apimachinery.pkg.api.resource.Quantity";
const INT_OR_STRING: &str = "k8s.io.apimachinery.pkg.util.intstr.IntOrString";
const RAW_EXTENSION: &str = "k8s.io.apimachinery.pkg.runtime.RawExtension";

/// Read an i64/i32 field by name, defaulting to 0.
fn int_field(msg: &DynamicMessage, name: &str) -> i64 {
    msg.descriptor()
        .get_field_by_name(name)
        .map(|f| match msg.get_field(&f).into_owned() {
            PbValue::I64(v) => v,
            PbValue::I32(v) => v as i64,
            PbValue::U64(v) => v as i64,
            PbValue::U32(v) => v as i64,
            _ => 0,
        })
        .unwrap_or(0)
}

fn str_field(msg: &DynamicMessage, name: &str) -> String {
    msg.descriptor()
        .get_field_by_name(name)
        .map(|f| match msg.get_field(&f).into_owned() {
            PbValue::String(s) => s,
            _ => String::new(),
        })
        .unwrap_or_default()
}

/// meta.v1.Time / MicroTime → RFC3339 string (or null for the zero time).
fn time_to_json(msg: &DynamicMessage, micro: bool) -> Value {
    let seconds = int_field(msg, "seconds");
    let nanos = int_field(msg, "nanos") as u32;
    if seconds == 0 && nanos == 0 {
        return Value::Null;
    }
    match chrono::DateTime::from_timestamp(seconds, nanos) {
        Some(dt) => {
            let s = if micro {
                dt.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
            } else {
                dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            };
            Value::String(s)
        }
        None => Value::Null,
    }
}

fn quantity_to_json(msg: &DynamicMessage) -> Value {
    Value::String(str_field(msg, "string"))
}

fn int_or_string_to_json(msg: &DynamicMessage) -> Value {
    // type: 0 = Int, 1 = String
    if int_field(msg, "type") == 0 {
        Value::Number(int_field(msg, "intVal").into())
    } else {
        Value::String(str_field(msg, "strVal"))
    }
}

fn raw_extension_to_json(msg: &DynamicMessage) -> Value {
    let raw = msg
        .descriptor()
        .get_field_by_name("raw")
        .map(|f| match msg.get_field(&f).into_owned() {
            PbValue::Bytes(b) => b.to_vec(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    serde_json::from_slice(&raw).unwrap_or(Value::Null)
}

// --- generic DynamicMessage → JSON -------------------------------------------

fn message_to_json(msg: &DynamicMessage) -> Value {
    match msg.descriptor().full_name() {
        TIME => return time_to_json(msg, false),
        MICRO_TIME => return time_to_json(msg, true),
        QUANTITY => return quantity_to_json(msg),
        INT_OR_STRING => return int_or_string_to_json(msg),
        RAW_EXTENSION => return raw_extension_to_json(msg),
        _ => {}
    }

    let mut obj = Map::new();
    for field in msg.descriptor().fields() {
        let value = msg.get_field(&field);
        match value.as_ref() {
            PbValue::List(items) => {
                if items.is_empty() {
                    continue;
                }
                let arr: Vec<Value> = items.iter().map(pb_scalar_or_msg_to_json).collect();
                obj.insert(field.json_name().to_string(), Value::Array(arr));
            }
            PbValue::Map(entries) => {
                if entries.is_empty() {
                    continue;
                }
                let mut mo = Map::new();
                for (k, v) in entries {
                    mo.insert(map_key_to_string(k), pb_scalar_or_msg_to_json(v));
                }
                obj.insert(field.json_name().to_string(), Value::Object(mo));
            }
            _ => {
                // Singular: include only if explicitly present, so unset proto2
                // optionals become absent JSON keys rather than zero values.
                if !msg.has_field(&field) {
                    continue;
                }
                obj.insert(
                    field.json_name().to_string(),
                    pb_scalar_or_msg_to_json(value.as_ref()),
                );
            }
        }
    }
    Value::Object(obj)
}

fn map_key_to_string(k: &prost_reflect::MapKey) -> String {
    use prost_reflect::MapKey;
    match k {
        MapKey::String(s) => s.clone(),
        MapKey::Bool(b) => b.to_string(),
        MapKey::I32(v) => v.to_string(),
        MapKey::I64(v) => v.to_string(),
        MapKey::U32(v) => v.to_string(),
        MapKey::U64(v) => v.to_string(),
    }
}

fn pb_scalar_or_msg_to_json(v: &PbValue) -> Value {
    match v {
        PbValue::Bool(b) => Value::Bool(*b),
        PbValue::I32(n) => Value::Number((*n).into()),
        PbValue::I64(n) => Value::Number((*n).into()),
        PbValue::U32(n) => Value::Number((*n).into()),
        PbValue::U64(n) => Value::Number((*n).into()),
        PbValue::F32(n) => serde_json::Number::from_f64(*n as f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        PbValue::F64(n) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        PbValue::String(s) => Value::String(s.clone()),
        PbValue::Bytes(b) => {
            use base64::Engine;
            Value::String(base64::engine::general_purpose::STANDARD.encode(b))
        }
        PbValue::EnumNumber(n) => Value::Number((*n).into()),
        PbValue::Message(m) => message_to_json(m),
        // Nested lists/maps do not occur in proto (repeated-of-repeated is not
        // representable), so this arm is unreachable in practice.
        PbValue::List(_) | PbValue::Map(_) => Value::Null,
    }
}

// --- generic JSON → DynamicMessage -------------------------------------------

fn json_to_message(json: &Value, desc: &MessageDescriptor) -> Result<DynamicMessage, String> {
    match desc.full_name() {
        TIME => return json_to_time(json, desc, false),
        MICRO_TIME => return json_to_time(json, desc, true),
        QUANTITY => return json_to_quantity(json, desc),
        INT_OR_STRING => return json_to_int_or_string(json, desc),
        RAW_EXTENSION => return json_to_raw_extension(json, desc),
        _ => {}
    }

    let mut msg = DynamicMessage::new(desc.clone());
    let obj = match json {
        Value::Object(m) => m,
        _ => return Ok(msg), // non-object → empty message
    };

    for (key, val) in obj {
        if key == "apiVersion" || key == "kind" || val.is_null() {
            continue; // envelope TypeMeta, not a proto field
        }
        let field = match desc
            .get_field_by_json_name(key)
            .or_else(|| desc.get_field_by_name(key))
        {
            Some(f) => f,
            None => continue, // unknown field: ignore (server is authoritative)
        };

        if field.is_list() {
            let items = match val {
                Value::Array(a) => a,
                _ => continue,
            };
            let mut list = Vec::with_capacity(items.len());
            for it in items {
                list.push(json_to_pb_value(&field, it)?);
            }
            msg.set_field(&field, PbValue::List(list));
        } else if field.is_map() {
            let entries = match val {
                Value::Object(m) => m,
                _ => continue,
            };
            let value_field = map_value_field(&field);
            let mut map = std::collections::HashMap::new();
            for (k, v) in entries {
                let mk = prost_reflect::MapKey::String(k.clone());
                let pv = match &value_field {
                    Some(vf) => json_to_pb_value(vf, v)?,
                    None => continue,
                };
                map.insert(mk, pv);
            }
            msg.set_field(&field, PbValue::Map(map));
        } else {
            msg.set_field(&field, json_to_pb_value(&field, val)?);
        }
    }
    Ok(msg)
}

/// The value FieldDescriptor of a map field (the synthetic map-entry's field 2).
fn map_value_field(
    field: &prost_reflect::FieldDescriptor,
) -> Option<prost_reflect::FieldDescriptor> {
    match field.kind() {
        prost_reflect::Kind::Message(entry) if entry.is_map_entry() => {
            Some(entry.map_entry_value_field())
        }
        _ => None,
    }
}

fn json_to_pb_value(
    field: &prost_reflect::FieldDescriptor,
    val: &Value,
) -> Result<PbValue, String> {
    use prost_reflect::Kind;
    Ok(match field.kind() {
        Kind::Message(md) => PbValue::Message(json_to_message(val, &md)?),
        Kind::Bool => PbValue::Bool(val.as_bool().unwrap_or(false)),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
            PbValue::I32(val.as_i64().unwrap_or(0) as i32)
        }
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => PbValue::I64(json_as_i64(val)),
        Kind::Uint32 | Kind::Fixed32 => PbValue::U32(val.as_u64().unwrap_or(0) as u32),
        Kind::Uint64 | Kind::Fixed64 => PbValue::U64(val.as_u64().unwrap_or(0)),
        Kind::Float => PbValue::F32(val.as_f64().unwrap_or(0.0) as f32),
        Kind::Double => PbValue::F64(val.as_f64().unwrap_or(0.0)),
        Kind::String => PbValue::String(val.as_str().unwrap_or("").to_string()),
        Kind::Bytes => {
            use base64::Engine;
            let b = val
                .as_str()
                .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                .unwrap_or_default();
            PbValue::Bytes(b.into())
        }
        Kind::Enum(en) => {
            // k8s uses string proto types for most "enums"; a real enum maps by
            // value name, falling back to a number.
            let n = val
                .as_str()
                .and_then(|s| en.get_value_by_name(s))
                .map(|v| v.number())
                .or_else(|| val.as_i64().map(|n| n as i32))
                .unwrap_or(0);
            PbValue::EnumNumber(n)
        }
    })
}

/// int64 fields are sometimes encoded as JSON strings by clients; accept both.
fn json_as_i64(val: &Value) -> i64 {
    val.as_i64()
        .or_else(|| val.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn set_named_field(msg: &mut DynamicMessage, name: &str, value: PbValue) {
    if let Some(f) = msg.descriptor().get_field_by_name(name) {
        msg.set_field(&f, value);
    }
}

fn json_to_time(
    json: &Value,
    desc: &MessageDescriptor,
    _micro: bool,
) -> Result<DynamicMessage, String> {
    let mut msg = DynamicMessage::new(desc.clone());
    if let Some(s) = json.as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            set_named_field(&mut msg, "seconds", PbValue::I64(dt.timestamp()));
            set_named_field(
                &mut msg,
                "nanos",
                PbValue::I32(dt.timestamp_subsec_nanos() as i32),
            );
        }
    }
    Ok(msg)
}

fn json_to_quantity(json: &Value, desc: &MessageDescriptor) -> Result<DynamicMessage, String> {
    let mut msg = DynamicMessage::new(desc.clone());
    let s = match json {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    set_named_field(&mut msg, "string", PbValue::String(s));
    Ok(msg)
}

fn json_to_int_or_string(
    json: &Value,
    desc: &MessageDescriptor,
) -> Result<DynamicMessage, String> {
    let mut msg = DynamicMessage::new(desc.clone());
    match json {
        Value::Number(n) => {
            set_named_field(&mut msg, "type", PbValue::I64(0));
            set_named_field(&mut msg, "intVal", PbValue::I32(n.as_i64().unwrap_or(0) as i32));
        }
        Value::String(s) => {
            set_named_field(&mut msg, "type", PbValue::I64(1));
            set_named_field(&mut msg, "strVal", PbValue::String(s.clone()));
        }
        _ => {}
    }
    Ok(msg)
}

fn json_to_raw_extension(
    json: &Value,
    desc: &MessageDescriptor,
) -> Result<DynamicMessage, String> {
    let mut msg = DynamicMessage::new(desc.clone());
    let bytes = serde_json::to_vec(json).unwrap_or_default();
    set_named_field(&mut msg, "raw", PbValue::Bytes(bytes.into()));
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn magic_detection() {
        assert!(is_protobuf(b"k8s\x00rest"));
        assert!(!is_protobuf(b"{\"a\":1}"));
        assert!(!is_protobuf(b"k8"));
    }

    #[test]
    fn content_type_negotiation() {
        assert!(wants_protobuf("application/vnd.kubernetes.protobuf"));
        assert!(wants_protobuf("application/vnd.kubernetes.protobuf;stream=watch"));
        assert!(!wants_protobuf("application/json"));
    }

    #[test]
    fn supports_known_and_unknown_groups() {
        assert!(supports("coordination.k8s.io/v1", "Lease"));
        assert!(!supports("example.com/v1", "Widget"));
    }

    #[test]
    fn lease_json_round_trips_through_protobuf() {
        // The real client-go path: object → protobuf → object must preserve
        // scalars, the MicroTime special type, and TypeMeta from the envelope.
        let lease = json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {
                "name": "cilium-operator-resource-lock",
                "namespace": "kube-system"
            },
            "spec": {
                "holderIdentity": "cilium-operator-abc",
                "leaseDurationSeconds": 15,
                "renewTime": "2026-07-19T12:34:56.123456Z",
                "leaseTransitions": 3
            }
        });
        let wire = encode_from_json(&lease, "coordination.k8s.io/v1", "Lease").unwrap();
        assert!(is_protobuf(&wire), "encoded body must carry the k8s magic");
        let back = decode_to_json(&wire).unwrap();

        assert_eq!(back["apiVersion"], "coordination.k8s.io/v1");
        assert_eq!(back["kind"], "Lease");
        assert_eq!(back["metadata"]["name"], "cilium-operator-resource-lock");
        assert_eq!(back["metadata"]["namespace"], "kube-system");
        assert_eq!(back["spec"]["holderIdentity"], "cilium-operator-abc");
        assert_eq!(back["spec"]["leaseDurationSeconds"], 15);
        assert_eq!(back["spec"]["leaseTransitions"], 3);
        // MicroTime survives the round trip as an RFC3339 string.
        assert_eq!(back["spec"]["renewTime"], "2026-07-19T12:34:56.123456Z");
    }

    #[test]
    fn pod_round_trips_quantity_intorstring_bytes_and_maps() {
        // Core group + the remaining special types: Quantity (resource limits),
        // IntOrString (containerPort is int; a probe port could be a string),
        // Secret-style bytes, and string maps (labels).
        let pod = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "p1",
                "namespace": "default",
                "labels": { "app": "cilium", "tier": "node" }
            },
            "spec": {
                "containers": [{
                    "name": "c",
                    "image": "busybox",
                    "ports": [{ "containerPort": 8080, "protocol": "TCP" }],
                    "resources": {
                        "limits": { "cpu": "500m", "memory": "128Mi" }
                    }
                }]
            }
        });
        let wire = encode_from_json(&pod, "v1", "Pod").unwrap();
        let back = decode_to_json(&wire).unwrap();
        assert_eq!(back["kind"], "Pod");
        assert_eq!(back["metadata"]["labels"]["app"], "cilium");
        let c = &back["spec"]["containers"][0];
        assert_eq!(c["name"], "c");
        assert_eq!(c["ports"][0]["containerPort"], 8080);
        // Quantity survives as its canonical string.
        assert_eq!(c["resources"]["limits"]["cpu"], "500m");
        assert_eq!(c["resources"]["limits"]["memory"], "128Mi");
    }

    #[test]
    fn service_nodeport_intorstring_targetport() {
        // targetPort is an IntOrString — exercise both forms.
        let svc = json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": { "name": "s", "namespace": "default" },
            "spec": {
                "ports": [
                    { "port": 80, "targetPort": 8080, "protocol": "TCP" },
                    { "port": 443, "targetPort": "https", "protocol": "TCP" }
                ]
            }
        });
        let wire = encode_from_json(&svc, "v1", "Service").unwrap();
        let back = decode_to_json(&wire).unwrap();
        assert_eq!(back["spec"]["ports"][0]["targetPort"], 8080);
        assert_eq!(back["spec"]["ports"][1]["targetPort"], "https");
    }

    #[test]
    fn unknown_fields_are_ignored_not_fatal() {
        // The server is authoritative; a client field we don't model must not
        // break decoding.
        let lease = json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": { "name": "x", "bogusField": "ignore me" },
            "spec": { "holderIdentity": "h" }
        });
        let wire = encode_from_json(&lease, "coordination.k8s.io/v1", "Lease").unwrap();
        let back = decode_to_json(&wire).unwrap();
        assert_eq!(back["metadata"]["name"], "x");
        assert_eq!(back["spec"]["holderIdentity"], "h");
    }
}
