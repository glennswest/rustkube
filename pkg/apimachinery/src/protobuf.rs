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
    // apimachinery meta/v1 envelope types (DeleteOptions, the *Options request
    // bodies, Status) are sent with apiVersion "v1" or "meta.k8s.io/v1" and live
    // in the shared meta package, not any API group. Resolve them first.
    if matches!(
        kind,
        "DeleteOptions"
            | "CreateOptions"
            | "UpdateOptions"
            | "PatchOptions"
            | "GetOptions"
            | "ListOptions"
            | "Status"
    ) {
        let name = format!("k8s.io.apimachinery.pkg.apis.meta.v1.{kind}");
        return POOL.get_message_by_name(&name).map(|_| name);
    }

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
        ("policy", "v1") => "k8s.io.api.policy.v1",
        ("autoscaling", "v2") => "k8s.io.api.autoscaling.v2",
        ("scheduling.k8s.io", "v1") => "k8s.io.api.scheduling.v1",
        ("resource.k8s.io", "v1") => "k8s.io.api.resource.v1",
        ("admissionregistration.k8s.io", "v1") => "k8s.io.api.admissionregistration.v1",
        ("certificates.k8s.io", "v1") => "k8s.io.api.certificates.v1",
        ("authentication.k8s.io", "v1") => "k8s.io.api.authentication.v1",
        ("authorization.k8s.io", "v1") => "k8s.io.api.authorization.v1",
        ("node.k8s.io", "v1") => "k8s.io.api.node.v1",
        ("events.k8s.io", "v1") => "k8s.io.api.events.v1",
        ("apiextensions.k8s.io", "v1") => {
            "k8s.io.apiextensions_apiserver.pkg.apis.apiextensions.v1"
        }
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
///
/// `hint_api_version`/`hint_kind` come from the request URL (the endpoint's GVK)
/// and are used when the protobuf envelope's TypeMeta is empty — which is the
/// common case for typed client-go clients (e.g. the apiextensions clientset
/// creating a CRD leaves TypeMeta blank and expects the server to know the type
/// from the path). Pass empty strings when there is no hint.
pub fn decode_to_json(
    body: &[u8],
    hint_api_version: &str,
    hint_kind: &str,
) -> Result<Value, String> {
    if !is_protobuf(body) {
        return Err("body is not k8s protobuf (missing magic)".into());
    }
    let unknown = Unknown::decode(&body[4..]).map_err(|e| format!("bad Unknown envelope: {e}"))?;
    let tm = unknown.type_meta.unwrap_or_default();
    let mut api_version = tm.api_version.unwrap_or_default();
    let mut kind = tm.kind.unwrap_or_default();
    // Fall back to the endpoint's GVK when the envelope doesn't carry one.
    if api_version.is_empty() {
        api_version = hint_api_version.to_string();
    }
    if kind.is_empty() {
        kind = hint_kind.to_string();
    }

    let msg_name = message_name(&api_version, &kind)
        .ok_or_else(|| format!("no protobuf schema for {api_version:?} {kind:?}"))?;
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

// apiextensions CRD-schema types with custom JSON marshaling (#34). Without
// these, a CRD's openAPIV3Schema round-trips into the wrong JSON shape.
const AEXT: &str = "k8s.io.apiextensions_apiserver.pkg.apis.apiextensions.v1";
const AEXT_JSON: &str = "k8s.io.apiextensions_apiserver.pkg.apis.apiextensions.v1.JSON";
const AEXT_PROPS: &str = "k8s.io.apiextensions_apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";

/// Fields whose Kubernetes JSON name is not the proto `json_name`.
///
/// Everywhere else the proto field name *is* the Go JSON tag, but
/// JSONSchemaProps names its extensions `x-kubernetes-*` and its refs `$ref`
/// and `$schema`. Decoding them under the proto names stored every CRD
/// created over protobuf with `xKubernetesPreserveUnknownFields` in its
/// schema — a key no JSON client knows, so a JSON read of the CRD said the
/// field was unset (#67, the CRD status-subresource conformance spec).
const K8S_JSON_NAMES: &[(&str, &str, &str)] = &[
    (AEXT_PROPS, "ref", "$ref"),
    (AEXT_PROPS, "schema", "$schema"),
    (AEXT_PROPS, "xKubernetesPreserveUnknownFields", "x-kubernetes-preserve-unknown-fields"),
    (AEXT_PROPS, "xKubernetesEmbeddedResource", "x-kubernetes-embedded-resource"),
    (AEXT_PROPS, "xKubernetesIntOrString", "x-kubernetes-int-or-string"),
    (AEXT_PROPS, "xKubernetesListMapKeys", "x-kubernetes-list-map-keys"),
    (AEXT_PROPS, "xKubernetesListType", "x-kubernetes-list-type"),
    (AEXT_PROPS, "xKubernetesMapType", "x-kubernetes-map-type"),
    (AEXT_PROPS, "xKubernetesValidations", "x-kubernetes-validations"),
    // Capitalised proto field names under lower-case JSON tags: a webhook
    // configuration read or written over protobuf lost every webhook (#64,
    // found by the inline-embed test).
    (ADMREG_VWC, "Webhooks", "webhooks"),
    (ADMREG_MWC, "Webhooks", "webhooks"),
    ("k8s.io.api.admissionregistration.v1.Validation", "Expression", "expression"),
    ("k8s.io.api.admissionregistration.v1.Variable", "Name", "name"),
    ("k8s.io.api.admissionregistration.v1.Variable", "Expression", "expression"),
];
const ADMREG_VWC: &str = "k8s.io.api.admissionregistration.v1.ValidatingWebhookConfiguration";
const ADMREG_MWC: &str = "k8s.io.api.admissionregistration.v1.MutatingWebhookConfiguration";

/// Go structs embedded with `json:",inline"`: in protobuf the embedded struct
/// is a field of its own, in JSON its fields sit beside the outer struct's.
/// Every one in the served groups (from k8s.io/api v0.36 `types.go`).
///
/// Without this a JSON object encoded to protobuf lost the embedded fields
/// (they matched no field of the outer message) and a protobuf body decoded
/// to JSON nested them under the proto name. A PV read over protobuf had no
/// `csi`, so upstream's snapshot-controller found "no CSI
/// PersistentVolumeSource" (#64); a pod's volumes lost their source, its
/// probes their handler, every `configMapKeyRef`/`secretKeyRef`/`envFrom`
/// and configMap/secret volume its `name`.
const INLINE: &[(&str, &str)] = &[
    ("k8s.io.api.core.v1.Volume", "volumeSource"),
    ("k8s.io.api.core.v1.PersistentVolumeSpec", "persistentVolumeSource"),
    ("k8s.io.api.core.v1.Probe", "handler"),
    ("k8s.io.api.core.v1.EphemeralContainer", "ephemeralContainerCommon"),
    ("k8s.io.api.core.v1.SecretProjection", "localObjectReference"),
    ("k8s.io.api.core.v1.ConfigMapVolumeSource", "localObjectReference"),
    ("k8s.io.api.core.v1.ConfigMapProjection", "localObjectReference"),
    ("k8s.io.api.core.v1.ConfigMapKeySelector", "localObjectReference"),
    ("k8s.io.api.core.v1.SecretKeySelector", "localObjectReference"),
    ("k8s.io.api.core.v1.ConfigMapEnvSource", "localObjectReference"),
    ("k8s.io.api.core.v1.SecretEnvSource", "localObjectReference"),
    ("k8s.io.api.networking.v1.IngressRule", "ingressRuleValue"),
    ("k8s.io.api.admissionregistration.v1.NamedRuleWithOperations", "ruleWithOperations"),
    ("k8s.io.api.admissionregistration.v1.RuleWithOperations", "rule"),
];

fn is_inline(msg: &str, field: &str) -> bool {
    INLINE.iter().any(|(m, f)| *m == msg && *f == field)
}

/// The JSON key for `field` of message `msg`, as Kubernetes spells it.
fn k8s_json_key(msg: &str, field: &prost_reflect::FieldDescriptor) -> String {
    K8S_JSON_NAMES
        .iter()
        .find(|(m, f, _)| *m == msg && *f == field.name())
        .map(|(_, _, j)| j.to_string())
        .unwrap_or_else(|| field.json_name().to_string())
}

/// The proto field name behind a Kubernetes-spelled JSON key, when the two differ.
fn proto_field_for_k8s_key(msg: &str, key: &str) -> Option<&'static str> {
    K8S_JSON_NAMES.iter().find(|(m, _, j)| *m == msg && *j == key).map(|(_, f, _)| *f)
}
const AEXT_PROPS_OR_BOOL: &str =
    "k8s.io.apiextensions_apiserver.pkg.apis.apiextensions.v1.JSONSchemaPropsOrBool";
const AEXT_PROPS_OR_ARRAY: &str =
    "k8s.io.apiextensions_apiserver.pkg.apis.apiextensions.v1.JSONSchemaPropsOrArray";
const AEXT_PROPS_OR_STRING_ARRAY: &str =
    "k8s.io.apiextensions_apiserver.pkg.apis.apiextensions.v1.JSONSchemaPropsOrStringArray";

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

/// Read a nested message field by name and convert it to JSON, if present.
fn msg_field_to_json(msg: &DynamicMessage, name: &str) -> Option<Value> {
    let f = msg.descriptor().get_field_by_name(name)?;
    if !msg.has_field(&f) {
        return None;
    }
    match msg.get_field(&f).into_owned() {
        PbValue::Message(m) => Some(message_to_json(&m)),
        _ => None,
    }
}

fn bool_field(msg: &DynamicMessage, name: &str) -> bool {
    msg.descriptor()
        .get_field_by_name(name)
        .map(|f| matches!(msg.get_field(&f).into_owned(), PbValue::Bool(true)))
        .unwrap_or(false)
}

/// Convert a repeated field to a JSON array, mapping each element.
fn list_field_to_json(msg: &DynamicMessage, name: &str) -> Vec<Value> {
    msg.descriptor()
        .get_field_by_name(name)
        .and_then(|f| match msg.get_field(&f).into_owned() {
            PbValue::List(items) => Some(items.iter().map(pb_scalar_or_msg_to_json).collect()),
            _ => None,
        })
        .unwrap_or_default()
}

fn message_to_json(msg: &DynamicMessage) -> Value {
    match msg.descriptor().full_name() {
        TIME => return time_to_json(msg, false),
        MICRO_TIME => return time_to_json(msg, true),
        QUANTITY => return quantity_to_json(msg),
        INT_OR_STRING => return int_or_string_to_json(msg),
        RAW_EXTENSION => return raw_extension_to_json(msg),
        // JSON: raw bytes hold an embedded JSON value.
        AEXT_JSON => return raw_extension_to_json(msg),
        // Union types: marshal as the schema object when present, else the
        // scalar/array alternative (matches k8s custom (un)marshaling).
        AEXT_PROPS_OR_BOOL => {
            return msg_field_to_json(msg, "schema").unwrap_or(Value::Bool(bool_field(msg, "allows")));
        }
        AEXT_PROPS_OR_ARRAY => {
            let arr = list_field_to_json(msg, "jSONSchemas");
            return if !arr.is_empty() {
                Value::Array(arr)
            } else {
                msg_field_to_json(msg, "schema").unwrap_or(Value::Null)
            };
        }
        AEXT_PROPS_OR_STRING_ARRAY => {
            let arr = list_field_to_json(msg, "property");
            return if !arr.is_empty() {
                Value::Array(arr)
            } else {
                msg_field_to_json(msg, "schema").unwrap_or(Value::Null)
            };
        }
        _ => {}
    }

    let mut obj = Map::new();
    let msg_name = msg.descriptor().full_name().to_string();
    for field in msg.descriptor().fields() {
        let value = msg.get_field(&field);
        match value.as_ref() {
            PbValue::List(items) => {
                if items.is_empty() {
                    continue;
                }
                let arr: Vec<Value> = items.iter().map(pb_scalar_or_msg_to_json).collect();
                obj.insert(k8s_json_key(&msg_name, &field), Value::Array(arr));
            }
            PbValue::Map(entries) => {
                if entries.is_empty() {
                    continue;
                }
                let mut mo = Map::new();
                for (k, v) in entries {
                    mo.insert(map_key_to_string(k), pb_scalar_or_msg_to_json(v));
                }
                obj.insert(k8s_json_key(&msg_name, &field), Value::Object(mo));
            }
            _ => {
                // Singular: include only if explicitly present, so unset proto2
                // optionals become absent JSON keys rather than zero values.
                if !msg.has_field(&field) {
                    continue;
                }
                // An inline embed: its fields go beside ours, as in Go's JSON.
                if is_inline(&msg_name, field.name()) {
                    if let Value::Object(inner) = pb_scalar_or_msg_to_json(value.as_ref()) {
                        for (k, v) in inner {
                            obj.entry(k).or_insert(v);
                        }
                    }
                    continue;
                }
                obj.insert(
                    k8s_json_key(&msg_name, &field),
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

// --- unknown fields (server-side field validation, #122) ----------------------

/// The fields of a built-in object's JSON that its type does not have, as
/// paths (`spec.unknownField`, `spec.template.spec.containers[0].foo`,
/// `metadata.unknownMeta`) — what `fieldValidation=Strict` refuses and `Warn`
/// reports. The type comes from the vendored descriptors, matched exactly as
/// [`encode_from_json`] matches keys (Kubernetes' JSON names, inline embeds),
/// and the opaque types (times, quantities, IntOrString, RawExtension,
/// `managedFields`' FieldsV1, raw JSON) are not looked into. `None` when the
/// kind has no descriptor, and for CustomResourceDefinitions, whose schema
/// keys are not checked here.
pub fn unknown_fields(json: &Value, api_version: &str, kind: &str) -> Option<Vec<String>> {
    let name = message_name(api_version, kind)?;
    if name.starts_with(AEXT) {
        return None;
    }
    let desc = POOL.get_message_by_name(&name)?;
    let mut out = Vec::new();
    if let Value::Object(top) = json {
        // apiVersion/kind are the envelope's TypeMeta, not the message's.
        let body: Map<String, Value> =
            top.iter().filter(|(k, _)| *k != "apiVersion" && *k != "kind").map(|(k, v)| (k.clone(), v.clone())).collect();
        unknown_in(&Value::Object(body), &desc, "", &mut out);
    }
    Some(out)
}

const FIELDS_V1: &str = "k8s.io.apimachinery.pkg.apis.meta.v1.FieldsV1";

fn unknown_in(json: &Value, desc: &MessageDescriptor, path: &str, out: &mut Vec<String>) {
    let opaque = [TIME, MICRO_TIME, QUANTITY, INT_OR_STRING, RAW_EXTENSION, AEXT_JSON, FIELDS_V1];
    if opaque.contains(&desc.full_name()) || desc.full_name().starts_with(AEXT) {
        return;
    }
    let Value::Object(obj) = json else { return };
    let join = |k: &str| if path.is_empty() { k.to_string() } else { format!("{path}.{k}") };
    let mut rest = Map::new();
    for (key, val) in obj {
        let field = desc
            .get_field_by_json_name(key)
            .or_else(|| desc.get_field_by_name(key))
            .or_else(|| proto_field_for_k8s_key(desc.full_name(), key).and_then(|f| desc.get_field_by_name(f)));
        let Some(field) = field else {
            rest.insert(key.clone(), val.clone());
            continue;
        };
        let prost_reflect::Kind::Message(inner) = field.kind() else { continue };
        if field.is_map() {
            if let (Some(vf), Value::Object(m)) = (map_value_field(&field), val) {
                if let prost_reflect::Kind::Message(vm) = vf.kind() {
                    for (k, v) in m {
                        unknown_in(v, &vm, &format!("{}[{k}]", join(key)), out);
                    }
                }
            }
        } else if field.is_list() {
            if let Value::Array(items) = val {
                for (i, it) in items.iter().enumerate() {
                    unknown_in(it, &inner, &format!("{}[{i}]", join(key)), out);
                }
            }
        } else {
            unknown_in(val, &inner, &join(key), out);
        }
    }
    // What no field here has may be an inline embed's; what it does not have
    // either is unknown.
    let inline: Vec<_> = desc.fields().filter(|f| is_inline(desc.full_name(), f.name())).collect();
    if inline.is_empty() {
        out.extend(rest.keys().map(|k| join(k)));
        return;
    }
    for field in inline {
        if let prost_reflect::Kind::Message(inner) = field.kind() {
            unknown_in(&Value::Object(rest.clone()), &inner, path, out);
        }
    }
}

// --- generic JSON → DynamicMessage -------------------------------------------

fn json_to_message(json: &Value, desc: &MessageDescriptor) -> Result<DynamicMessage, String> {
    match desc.full_name() {
        TIME => return json_to_time(json, desc, false),
        MICRO_TIME => return json_to_time(json, desc, true),
        QUANTITY => return json_to_quantity(json, desc),
        INT_OR_STRING => return json_to_int_or_string(json, desc),
        RAW_EXTENSION | AEXT_JSON => return json_to_raw_extension(json, desc),
        AEXT_PROPS_OR_BOOL => return json_to_props_or_bool(json, desc),
        AEXT_PROPS_OR_ARRAY => return json_to_props_or_array(json, desc),
        AEXT_PROPS_OR_STRING_ARRAY => return json_to_props_or_string_array(json, desc),
        _ => {}
    }

    let mut msg = DynamicMessage::new(desc.clone());
    let obj = match json {
        Value::Object(m) => m,
        _ => return Ok(msg), // non-object → empty message
    };

    // Keys no field of this message has: an inline embed's (below).
    let mut rest = Map::new();
    for (key, val) in obj {
        // `apiVersion` and `kind` are NOT skipped by name. At the top level they
        // are the envelope's TypeMeta, and the object's message has no such
        // fields, so the lookup below ignores them anyway. Nested, they are
        // real fields — `roleRef.kind`, `subjects[].kind`, every
        // `ownerReferences[].kind`/`apiVersion` and object reference — and
        // skipping them by name emptied every one in every protobuf response.
        // `oc adm policy remove-role-from-user` could never find the binding
        // it had just added, because every roleRef read back kind-less (#69).
        if val.is_null() {
            continue;
        }
        let field = match desc
            .get_field_by_json_name(key)
            .or_else(|| desc.get_field_by_name(key))
            .or_else(|| proto_field_for_k8s_key(desc.full_name(), key).and_then(|f| desc.get_field_by_name(f)))
        {
            Some(f) => f,
            None => {
                // Unknown here: an inline embed's field, or ignored (the
                // server is authoritative).
                rest.insert(key.clone(), val.clone());
                continue;
            }
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
    // The embedded struct, from the keys that are its. A chain of embeds
    // (NamedRuleWithOperations → RuleWithOperations → Rule) resolves by
    // recursion: what this level does not know is passed down whole.
    for field in desc.fields().filter(|f| is_inline(desc.full_name(), f.name())) {
        if let prost_reflect::Kind::Message(inner) = field.kind() {
            let sub = json_to_message(&Value::Object(rest.clone()), &inner)?;
            msg.set_field(&field, PbValue::Message(sub));
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

/// Build a nested message field from JSON via the field's descriptor.
fn set_msg_field(
    msg: &mut DynamicMessage,
    name: &str,
    value: &Value,
) -> Result<(), String> {
    if let Some(f) = msg.descriptor().get_field_by_name(name) {
        if let prost_reflect::Kind::Message(md) = f.kind() {
            let nested = json_to_message(value, &md)?;
            msg.set_field(&f, PbValue::Message(nested));
        }
    }
    Ok(())
}

/// JSON bool → allows; JSON object → schema (CRD additionalProperties etc.).
fn json_to_props_or_bool(json: &Value, desc: &MessageDescriptor) -> Result<DynamicMessage, String> {
    let mut msg = DynamicMessage::new(desc.clone());
    match json {
        Value::Bool(b) => set_named_field(&mut msg, "allows", PbValue::Bool(*b)),
        other => set_msg_field(&mut msg, "schema", other)?,
    }
    Ok(msg)
}

/// JSON array → jSONSchemas; JSON object → schema (CRD `items`).
fn json_to_props_or_array(json: &Value, desc: &MessageDescriptor) -> Result<DynamicMessage, String> {
    let mut msg = DynamicMessage::new(desc.clone());
    match json {
        Value::Array(items) => {
            if let Some(f) = msg.descriptor().get_field_by_name("jSONSchemas") {
                let mut list = Vec::with_capacity(items.len());
                for it in items {
                    list.push(json_to_pb_value(&f, it)?);
                }
                msg.set_field(&f, PbValue::List(list));
            }
        }
        other => set_msg_field(&mut msg, "schema", other)?,
    }
    Ok(msg)
}

/// JSON string array → property; JSON object → schema (CRD `dependencies`).
fn json_to_props_or_string_array(
    json: &Value,
    desc: &MessageDescriptor,
) -> Result<DynamicMessage, String> {
    let mut msg = DynamicMessage::new(desc.clone());
    match json {
        Value::Array(items) => {
            if let Some(f) = msg.descriptor().get_field_by_name("property") {
                let list = items
                    .iter()
                    .map(|v| PbValue::String(v.as_str().unwrap_or("").to_string()))
                    .collect();
                msg.set_field(&f, PbValue::List(list));
            }
        }
        other => set_msg_field(&mut msg, "schema", other)?,
    }
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
    fn meta_options_resolve_and_roundtrip() {
        // DeleteOptions (and the other meta/v1 request envelopes) must resolve to
        // the apimachinery meta package regardless of the "v1"/"meta.k8s.io/v1"
        // apiVersion the client uses — helm/client-go send protobuf DeleteOptions.
        assert!(supports("v1", "DeleteOptions"));
        assert!(supports("meta.k8s.io/v1", "DeleteOptions"));
        let opts = json!({
            "apiVersion": "meta.k8s.io/v1", "kind": "DeleteOptions",
            "gracePeriodSeconds": 30, "propagationPolicy": "Foreground"
        });
        let bytes = encode_from_json(&opts, "v1", "DeleteOptions").unwrap();
        let back = decode_to_json(&bytes, "v1", "DeleteOptions").unwrap();
        assert_eq!(back["gracePeriodSeconds"], 30);
        assert_eq!(back["propagationPolicy"], "Foreground");
    }

    #[test]
    fn supports_known_and_unknown_groups() {
        assert!(supports("coordination.k8s.io/v1", "Lease"));
        assert!(!supports("example.com/v1", "Widget"));
    }

    #[test]
    fn dra_objects_round_trip_through_protobuf() {
        // resource.k8s.io/v1 (#137): client-go's typed DRA clients speak
        // protobuf. A DeviceClass with a CEL selector, and a ResourceClaim
        // with an exact request and an allocation in its status.
        let class = json!({
            "apiVersion": "resource.k8s.io/v1", "kind": "DeviceClass",
            "metadata": {"name": "gpu.example.com"},
            "spec": {"selectors": [{"cel": {"expression": "device.driver == \"gpu.example.com\""}}]}
        });
        let back = decode_to_json(&encode_from_json(&class, "resource.k8s.io/v1", "DeviceClass").unwrap(), "", "").unwrap();
        assert_eq!(back["kind"], "DeviceClass");
        assert_eq!(back["spec"]["selectors"][0]["cel"]["expression"], "device.driver == \"gpu.example.com\"");

        let claim = json!({
            "apiVersion": "resource.k8s.io/v1", "kind": "ResourceClaim",
            "metadata": {"name": "c1", "namespace": "default"},
            "spec": {"devices": {"requests": [{"name": "gpu", "exactly": {"deviceClassName": "gpu.example.com", "count": 2}}]}},
            "status": {"allocation": {"devices": {"results": [
                {"request": "gpu", "driver": "gpu.example.com", "pool": "node-a", "device": "gpu-0"}]}}}
        });
        let back = decode_to_json(&encode_from_json(&claim, "resource.k8s.io/v1", "ResourceClaim").unwrap(), "", "").unwrap();
        assert_eq!(back["spec"]["devices"]["requests"][0]["exactly"]["count"], 2);
        assert_eq!(back["status"]["allocation"]["devices"]["results"][0]["device"], "gpu-0");
    }

    #[test]
    fn unknown_fields_of_a_typed_object_are_found_by_path() {
        // The conformance FieldValidation body (#122).
        let d = json!({"apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "my-dep", "unknownMeta": "foo", "labels": {"app": "nginx"},
                         "managedFields": [{"manager": "m", "fieldsV1": {"f:spec": {"f:replicas": {}}}}]},
            "spec": {"unknownField": "foo", "replicas": 2, "selector": {"matchLabels": {"app": "nginx"}},
                "template": {"metadata": {"labels": {"app": "nginx"}},
                    "spec": {"containers": [{"name": "nginx", "image": "nginx", "bogus": 1,
                        "resources": {"requests": {"cpu": "100m"}}, "ports": [{"containerPort": 80}]}]}},
                "strategy": {"rollingUpdate": {"maxSurge": "25%"}}},
            "status": {}});
        let u = unknown_fields(&d, "apps/v1", "Deployment").unwrap();
        assert_eq!(u, vec!["metadata.unknownMeta", "spec.template.spec.containers[0].bogus", "spec.unknownField"]);
        // A clean object, every opaque type in it, has none.
        let mut clean = d.clone();
        clean["metadata"].as_object_mut().unwrap().remove("unknownMeta");
        clean["spec"].as_object_mut().unwrap().remove("unknownField");
        clean["spec"]["template"]["spec"]["containers"][0].as_object_mut().unwrap().remove("bogus");
        assert_eq!(unknown_fields(&clean, "apps/v1", "Deployment").unwrap(), Vec::<String>::new());
        // An inline embed's keys are known (ValidatingWebhook rules).
        let vwc = json!({"apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingWebhookConfiguration",
            "metadata": {"name": "w"}, "webhooks": [{"name": "a.b.c", "rules": [{"operations": ["CREATE"],
            "apiGroups": [""], "apiVersions": ["v1"], "resources": ["pods"], "scope": "*"}], "nope": true}]});
        assert_eq!(unknown_fields(&vwc, "admissionregistration.k8s.io/v1", "ValidatingWebhookConfiguration").unwrap(),
                   vec!["webhooks[0].nope"]);
        // A 1.33+ field is known (the descriptors are release-1.36).
        let pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"},
            "spec": {"resources": {"limits": {"cpu": "1"}}, "containers": [{"name": "c", "image": "i"}]}});
        assert_eq!(unknown_fields(&pod, "v1", "Pod").unwrap(), Vec::<String>::new());
        assert!(unknown_fields(&json!({}), "apiextensions.k8s.io/v1", "CustomResourceDefinition").is_none());
        assert!(unknown_fields(&json!({}), "example.com/v1", "Thing").is_none());
    }

    #[test]
    fn servicecidr_and_ipaddress_round_trip_through_protobuf() {
        // networking.k8s.io/v1 (#134), re-vendored from release-1.36: the
        // conformance spec's IPAddress create failed for want of a schema.
        let cidr = json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "ServiceCIDR",
            "metadata": {"name": "kubernetes"},
            "spec": {"cidrs": ["10.96.0.0/12"]},
            "status": {"conditions": [{"type": "Ready", "status": "True", "reason": "", "message": "ready",
                                       "lastTransitionTime": "2026-10-07T00:00:00Z"}]}
        });
        let back = decode_to_json(&encode_from_json(&cidr, "networking.k8s.io/v1", "ServiceCIDR").unwrap(), "", "").unwrap();
        assert_eq!(back["kind"], "ServiceCIDR");
        assert_eq!(back["spec"]["cidrs"][0], "10.96.0.0/12");
        assert_eq!(back["status"]["conditions"][0]["type"], "Ready");
        let ip = json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "IPAddress",
            "metadata": {"name": "10.96.0.10"},
            "spec": {"parentRef": {"group": "", "resource": "services", "namespace": "kube-system", "name": "dns"}}
        });
        let back = decode_to_json(&encode_from_json(&ip, "networking.k8s.io/v1", "IPAddress").unwrap(), "", "").unwrap();
        assert_eq!(back["spec"]["parentRef"]["name"], "dns");
        assert_eq!(back["metadata"]["name"], "10.96.0.10");
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
        let back = decode_to_json(&wire, "", "").unwrap();

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

    /// `oc adm policy remove-role-from-user` reads RoleBindings as protobuf
    /// and matches on `roleRef` and `subjects` (#69): a nested `kind` must
    /// survive the encoder.
    #[test]
    fn rolebinding_list_round_trips_role_ref_and_subjects() {
        let list = json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "RoleBindingList",
            "metadata": { "resourceVersion": "42" },
            "items": [{
                "metadata": { "name": "edit", "namespace": "work", "uid": "",
                              "creationTimestamp": null, "generation": 0 },
                "roleRef": { "apiGroup": "rbac.authorization.k8s.io",
                             "kind": "ClusterRole", "name": "edit" },
                "subjects": [{ "apiGroup": "rbac.authorization.k8s.io",
                               "kind": "User", "name": "alice", "namespace": "" }]
            }]
        });
        let wire = encode_from_json(&list, "rbac.authorization.k8s.io/v1", "RoleBindingList").unwrap();
        let back = decode_to_json(&wire, "", "").unwrap();
        let item = &back["items"][0];
        assert_eq!(item["metadata"]["name"], "edit");
        assert_eq!(item["roleRef"]["kind"], "ClusterRole");
        assert_eq!(item["roleRef"]["name"], "edit");
        assert_eq!(item["subjects"][0]["kind"], "User");
        assert_eq!(item["subjects"][0]["name"], "alice");
    }

    /// An owner reference names its owner by apiVersion and kind; without
    /// them a client cannot tell what owns the object.
    #[test]
    fn owner_references_keep_api_version_and_kind() {
        let rs = json!({
            "apiVersion": "apps/v1", "kind": "ReplicaSet",
            "metadata": { "name": "web-1", "namespace": "demo", "ownerReferences": [{
                "apiVersion": "apps/v1", "kind": "Deployment", "name": "web",
                "uid": "u1", "controller": true }] },
        });
        let wire = encode_from_json(&rs, "apps/v1", "ReplicaSet").unwrap();
        let back = decode_to_json(&wire, "", "").unwrap();
        let owner = &back["metadata"]["ownerReferences"][0];
        assert_eq!(owner["apiVersion"], "apps/v1");
        assert_eq!(owner["kind"], "Deployment");
        assert_eq!(owner["uid"], "u1");
    }

    /// Go's `json:",inline"` embeds are fields of their own on the wire and
    /// flat in JSON (#64: a PV read over protobuf had no `csi`).
    #[test]
    fn inline_embeds_roundtrip_flat() {
        let pv = json!({"apiVersion": "v1", "kind": "PersistentVolume", "metadata": {"name": "pv"},
            "spec": {"capacity": {"storage": "1Gi"}, "accessModes": ["ReadWriteOnce"],
                     "csi": {"driver": "d.example", "volumeHandle": "vol-1"}}});
        let wire = encode_from_json(&pv, "v1", "PersistentVolume").unwrap();
        let back = decode_to_json(&wire, "", "").unwrap();
        assert_eq!(back["spec"]["csi"]["volumeHandle"], "vol-1");
        assert_eq!(back["spec"]["capacity"]["storage"], "1Gi");
        assert!(back["spec"]["persistentVolumeSource"].is_null(), "flattened, not nested");

        // On the wire it is where Go puts it: spec.persistentVolumeSource.csi.
        let desc = POOL.get_message_by_name("k8s.io.api.core.v1.PersistentVolumeSpec").unwrap();
        let spec = json_to_message(&pv["spec"], &desc).unwrap();
        let src = spec.get_field_by_name("persistentVolumeSource").unwrap();
        let src = src.as_message().unwrap();
        assert!(src.has_field_by_name("csi"));

        let pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"},
            "spec": {"containers": [{"name": "c", "image": "i",
                        "env": [{"name": "E", "valueFrom": {"configMapKeyRef": {"name": "cm", "key": "k"}}}],
                        "envFrom": [{"secretRef": {"name": "sec"}}],
                        "readinessProbe": {"httpGet": {"path": "/ready", "port": 8080}, "periodSeconds": 5}}],
                     "volumes": [{"name": "v", "configMap": {"name": "cm", "defaultMode": 420}}]}});
        let back = decode_to_json(&encode_from_json(&pod, "v1", "Pod").unwrap(), "", "").unwrap();
        let c = &back["spec"]["containers"][0];
        assert_eq!(c["env"][0]["valueFrom"]["configMapKeyRef"]["name"], "cm");
        assert_eq!(c["envFrom"][0]["secretRef"]["name"], "sec");
        assert_eq!(c["readinessProbe"]["httpGet"]["path"], "/ready");
        assert_eq!(c["readinessProbe"]["periodSeconds"], 5);
        assert_eq!(back["spec"]["volumes"][0]["name"], "v");
        assert_eq!(back["spec"]["volumes"][0]["configMap"]["name"], "cm");

        // A chain of embeds: NamedRuleWithOperations → RuleWithOperations → Rule.
        let vwc = json!({"apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingWebhookConfiguration",
            "metadata": {"name": "w"}, "webhooks": [{"name": "w.example", "sideEffects": "None", "admissionReviewVersions": ["v1"],
                "clientConfig": {"url": "https://x"},
                "rules": [{"operations": ["CREATE"], "apiGroups": [""], "apiVersions": ["v1"], "resources": ["pods"]}]}]});
        let back = decode_to_json(&encode_from_json(&vwc, "admissionregistration.k8s.io/v1", "ValidatingWebhookConfiguration").unwrap(), "", "").unwrap();
        let rule = &back["webhooks"][0]["rules"][0];
        assert_eq!(rule["operations"][0], "CREATE", "decoded: {back}");
        assert_eq!(rule["resources"][0], "pods", "decoded: {back}");
    }

    /// client-go sends TokenRequest and SubjectAccessReview as protobuf; they
    /// were refused with "no protobuf schema" (#67).
    #[test]
    fn token_request_and_sar_have_schemas() {
        let tr = json!({"apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest",
                        "spec": {"audiences": ["api"], "expirationSeconds": 3600}});
        let back = decode_to_json(&encode_from_json(&tr, "authentication.k8s.io/v1", "TokenRequest").unwrap(), "", "").unwrap();
        assert_eq!(back["spec"]["audiences"], json!(["api"]));
        assert_eq!(back["spec"]["expirationSeconds"], 3600);
        assert!(supports("authorization.k8s.io/v1", "SubjectAccessReview"));
        assert!(supports("node.k8s.io/v1", "RuntimeClass"));
        assert!(supports("events.k8s.io/v1", "Event"));
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
        let back = decode_to_json(&wire, "", "").unwrap();
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
        let back = decode_to_json(&wire, "", "").unwrap();
        assert_eq!(back["spec"]["ports"][0]["targetPort"], 8080);
        assert_eq!(back["spec"]["ports"][1]["targetPort"], "https");
    }

    #[test]
    fn crd_round_trips_including_schema_union_types() {
        // #34: cilium-operator creates CRDs via protobuf. The openAPIV3Schema
        // exercises the tricky union types — additionalProperties (bool),
        // items (single schema), and a JSON default.
        let crd = json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": { "name": "ciliumnetworkpolicies.cilium.io" },
            "spec": {
                "group": "cilium.io",
                "scope": "Namespaced",
                "names": { "plural": "ciliumnetworkpolicies", "kind": "CiliumNetworkPolicy" },
                "versions": [{
                    "name": "v2",
                    "served": true,
                    "storage": true,
                    "schema": { "openAPIV3Schema": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "spec": {
                                "type": "object",
                                "properties": {
                                    "endpointSelector": { "type": "object", "additionalProperties": false }
                                }
                            },
                            "tags": { "type": "array", "items": { "type": "string" } }
                        }
                    }}
                }]
            }
        });
        let wire = encode_from_json(&crd, "apiextensions.k8s.io/v1", "CustomResourceDefinition")
            .unwrap();
        let back = decode_to_json(&wire, "", "").unwrap();
        assert_eq!(back["kind"], "CustomResourceDefinition");
        assert_eq!(back["spec"]["group"], "cilium.io");
        let schema = &back["spec"]["versions"][0]["schema"]["openAPIV3Schema"];
        // additionalProperties: bool union survives both true and false.
        assert_eq!(schema["additionalProperties"], true);
        assert_eq!(
            schema["properties"]["spec"]["properties"]["endpointSelector"]["additionalProperties"],
            false
        );
        // items: single-schema union survives.
        assert_eq!(schema["properties"]["tags"]["items"]["type"], "string");
    }

    /// JSONSchemaProps' `x-kubernetes-*`, `$ref` and `$schema` keep their
    /// Kubernetes spelling through protobuf in both directions (#67); the
    /// proto spelling a pre-fix store holds still encodes.
    #[test]
    fn schema_extensions_keep_their_kubernetes_names() {
        let crd = |schema: Value| json!({
            "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
            "metadata": { "name": "foos.example.com" },
            "spec": { "group": "example.com", "scope": "Cluster",
                      "names": { "plural": "foos", "kind": "Foo" },
                      "versions": [{ "name": "v1", "served": true, "storage": true,
                                     "schema": { "openAPIV3Schema": schema } }] }
        });
        let schema_of = |v: &Value| v["spec"]["versions"][0]["schema"]["openAPIV3Schema"].clone();
        let sent = crd(json!({
            "type": "object", "$schema": "http://json-schema.org/draft-04/schema#",
            "x-kubernetes-preserve-unknown-fields": true,
            "properties": { "l": { "type": "array", "items": { "type": "object" },
                                   "x-kubernetes-list-type": "map",
                                   "x-kubernetes-list-map-keys": ["name"] },
                            "n": { "x-kubernetes-int-or-string": true } }
        }));
        let wire = encode_from_json(&sent, "apiextensions.k8s.io/v1", "CustomResourceDefinition").unwrap();
        let back = schema_of(&decode_to_json(&wire, "", "").unwrap());
        assert_eq!(back["x-kubernetes-preserve-unknown-fields"], true);
        assert_eq!(back["$schema"], "http://json-schema.org/draft-04/schema#");
        assert_eq!(back["properties"]["l"]["x-kubernetes-list-type"], "map");
        assert_eq!(back["properties"]["l"]["x-kubernetes-list-map-keys"], json!(["name"]));
        assert_eq!(back["properties"]["n"]["x-kubernetes-int-or-string"], true);
        assert!(back.get("xKubernetesPreserveUnknownFields").is_none());

        let old = crd(json!({ "type": "object", "xKubernetesPreserveUnknownFields": true }));
        let wire = encode_from_json(&old, "apiextensions.k8s.io/v1", "CustomResourceDefinition").unwrap();
        let back = schema_of(&decode_to_json(&wire, "", "").unwrap());
        assert_eq!(back["x-kubernetes-preserve-unknown-fields"], true);
    }

    #[test]
    fn blank_envelope_typemeta_falls_back_to_hint() {
        // #34: cilium's apiextensions client sends the CRD with an EMPTY
        // envelope TypeMeta and expects the server to resolve the type from the
        // endpoint. Simulate by encoding then blanking the envelope's TypeMeta.
        let crd = json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": { "name": "x.cilium.io" },
            "spec": { "group": "cilium.io" }
        });
        // Build a wire body with the object encoded but no TypeMeta in Unknown.
        let msg_name = message_name("apiextensions.k8s.io/v1", "CustomResourceDefinition").unwrap();
        let desc = POOL.get_message_by_name(&msg_name).unwrap();
        let raw = json_to_message(&crd, &desc).unwrap().encode_to_vec();
        let unknown = Unknown { type_meta: None, raw: Some(raw), content_encoding: None, content_type: None };
        let mut wire = MAGIC.to_vec();
        unknown.encode(&mut wire).unwrap();

        // No hint → fails (blank type), reproducing the bug.
        assert!(decode_to_json(&wire, "", "").is_err());
        // With the endpoint hint → decodes and stamps the GVK from the hint.
        let back = decode_to_json(&wire, "apiextensions.k8s.io/v1", "CustomResourceDefinition").unwrap();
        assert_eq!(back["apiVersion"], "apiextensions.k8s.io/v1");
        assert_eq!(back["kind"], "CustomResourceDefinition");
        assert_eq!(back["spec"]["group"], "cilium.io");
    }

    #[test]
    fn all_declared_groups_resolve() {
        for (av, kind) in [
            ("apiextensions.k8s.io/v1", "CustomResourceDefinition"),
            ("policy/v1", "PodDisruptionBudget"),
            ("autoscaling/v2", "HorizontalPodAutoscaler"),
            ("scheduling.k8s.io/v1", "PriorityClass"),
            ("admissionregistration.k8s.io/v1", "ValidatingWebhookConfiguration"),
            ("certificates.k8s.io/v1", "CertificateSigningRequest"),
            ("networking.k8s.io/v1", "NetworkPolicy"),
        ] {
            assert!(supports(av, kind), "protobuf must support {av} {kind}");
        }
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
        let back = decode_to_json(&wire, "", "").unwrap();
        assert_eq!(back["metadata"]["name"], "x");
        assert_eq!(back["spec"]["holderIdentity"], "h");
    }
}
