//! Kubernetes resource quantities — `100m`, `1Gi`, `1.5G`, `1e9`.
//!
//! One implementation, because there were three: `scheduler::filter`,
//! `scheduler::score` and `scheduler::preemption` each carried their own
//! `parse_memory_bytes`, and they had already drifted in what they accepted.
//! A parser that silently returns 0 for a quantity it does not recognise
//! decides scheduling and volume binding — a PVC asking for `1.5Gi` matching
//! a 0-byte volume is a bind that should not have happened — so the fallback
//! matters as much as the happy path.

/// Parse a quantity to bytes.
///
/// Accepts the binary suffixes (`Ki Mi Gi Ti Pi Ei`), the decimal ones
/// (`n u m k K M G T P E`), plain numbers, fractions (`1.5Gi`) and the
/// exponent form (`1e9`). An unparseable quantity is 0 — the caller reads
/// that as "asks for nothing", which fails closed for capacity comparisons.
pub fn parse_bytes(s: &str) -> u64 {
    parse_quantity(s).map(|v| v.max(0.0) as u64).unwrap_or(0)
}

/// A quantity's value in its base unit (`100m` → 0.1, `1k` → 1000, `1Gi` →
/// 1073741824), or None when it is not one.
pub fn parse_quantity(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Binary (power-of-two) suffixes first: "Mi" ends in 'i', so it must be
    // tested before the decimal "M".
    const BINARY: &[(&str, f64)] = &[
        ("Ki", 1024.0),
        ("Mi", 1_048_576.0),
        ("Gi", 1_073_741_824.0),
        ("Ti", 1_099_511_627_776.0),
        ("Pi", 1_125_899_906_842_624.0),
        ("Ei", 1_152_921_504_606_846_976.0),
    ];
    for (suffix, mult) in BINARY {
        if let Some(num) = s.strip_suffix(suffix) {
            return num.parse::<f64>().ok().map(|v| v * mult);
        }
    }
    const DECIMAL: &[(&str, f64)] = &[
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("k", 1e3),
        ("K", 1e3), // not canonical, but written by hand often enough
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
    ];
    for (suffix, mult) in DECIMAL {
        // `1e9` ends in no suffix but contains 'E'/'e'; only strip a suffix
        // when what is left still parses as a number.
        if let Some(num) = s.strip_suffix(suffix) {
            if let Ok(v) = num.parse::<f64>() {
                return Some(v * mult);
            }
        }
    }
    s.parse::<f64>().ok()
}

/// Parse a CPU quantity to millicores: `250m`, `2`, `0.5`, and the SI
/// suffixes a canonicalized number gets (`1000` is stored as `1k`, #365).
pub fn parse_cpu_millis(s: &str) -> u64 {
    let s = s.trim();
    if let Some(Ok(v)) = s.strip_suffix('m').map(str::parse::<f64>) {
        return v.max(0.0) as u64;
    }
    parse_quantity(s).map(|v| (v.max(0.0) * 1000.0).round() as u64).unwrap_or(0)
}

/// A CPU quantity written as a string or, as some writers store it, a JSON
/// number (`cpu: 2`), in millicores. Anything else is 0.
pub fn cpu_millis(v: &serde_json::Value) -> u64 {
    match v {
        serde_json::Value::String(s) => parse_cpu_millis(s),
        serde_json::Value::Number(n) => n.as_f64().map(|f| (f.max(0.0) * 1000.0).round() as u64).unwrap_or(0),
        _ => 0,
    }
}

/// Any other quantity, string or JSON number, in its base unit (bytes,
/// counts). Anything else is 0.
pub fn base_units(v: &serde_json::Value) -> u64 {
    match v {
        serde_json::Value::String(s) => parse_bytes(s),
        serde_json::Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)).unwrap_or(0),
        _ => 0,
    }
}

/// Render bytes back as a quantity, preferring the largest binary suffix that
/// divides exactly — `1073741824` becomes `1Gi`, not `1024Mi` or a raw count.
/// Status fields are read by people.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("Ei", 1 << 60),
        ("Pi", 1 << 50),
        ("Ti", 1 << 40),
        ("Gi", 1 << 30),
        ("Mi", 1 << 20),
        ("Ki", 1 << 10),
    ];
    for (suffix, size) in UNITS {
        if bytes >= *size && bytes % size == 0 {
            return format!("{}{suffix}", bytes / size);
        }
    }
    bytes.to_string()
}

/// The built-in resources whose objects carry quantities (#292), by the
/// store key's resource segment. Custom resources and CRDs are not here: a
/// CRD's schema holds numeric `default`/`minimum`s, and a CR's types are its
/// own.
pub const QUANTITY_RESOURCES: &[&str] = &[
    "pods", "podtemplates", "replicationcontrollers", "replicasets", "deployments",
    "statefulsets", "daemonsets", "jobs", "cronjobs", "nodes", "persistentvolumeclaims",
    "persistentvolumes", "resourcequotas", "limitranges", "runtimeclasses",
    "horizontalpodautoscalers", "csistoragecapacities", "resourceslices", "resourceclaims",
    "resourceclaimtemplates",
];

/// Maps whose values are all quantities: `resources.limits/requests`, a
/// Node's `capacity`/`allocatable`, a quota's `hard`/`used`, a LimitRange
/// item's bounds and defaults, a Pod's `overhead`, RuntimeClass `podFixed`, …
const QUANTITY_MAPS: &[&str] = &[
    "limits", "requests", "capacity", "allocatable", "hard", "used", "overhead", "podFixed",
    "allocatedResources", "min", "max", "default", "defaultRequest", "maxLimitRequestRatio",
];

/// Fields that are a single quantity, wherever they sit in those resources.
const QUANTITY_FIELDS: &[&str] = &["sizeLimit", "averageValue", "maximumVolumeSize"];

/// Every JSON number at a quantity path of a built-in `resource`'s object
/// rewritten as upstream's canonical string (#292). True when it changed.
///
/// `resource.Quantity` is always a string on the wire: kube-apiserver takes
/// `cpu: 1` (an unquoted YAML number) and answers `"1"`. Stored as given, the
/// number broke every typed client — k8s-openapi's `Quantity(String)` refused
/// the whole PodList (stormupdate's controller, on cilium's
/// `install-cni-binaries` `limits.cpu: 1`). Strings are left as written.
pub fn numbers_to_strings(resource: &str, obj: &mut serde_json::Value) -> bool {
    if !QUANTITY_RESOURCES.contains(&resource) {
        return false;
    }
    let mut path = Vec::new();
    walk(resource, obj, &mut path)
}

/// An array's items sit under this path segment, not under the array's own
/// name (#367): `spec.devices.requests[]` is a list of DRA device requests,
/// not a `requests` quantity map, and a request's `count: 2` is an integer.
const ITEM: &str = "[]";

fn walk(resource: &str, v: &mut serde_json::Value, path: &mut Vec<String>) -> bool {
    use serde_json::Value;
    let mut changed = false;
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if let Value::Number(n) = child {
                    if is_quantity(resource, path, k) {
                        *child = Value::String(canonical_number(n));
                        changed = true;
                    }
                    continue;
                }
                if is_opaque(resource, path, k) {
                    continue;
                }
                path.push(k.clone());
                changed |= walk(resource, child, path);
                path.pop();
            }
        }
        Value::Array(items) => {
            path.push(ITEM.into());
            for item in items {
                changed |= walk(resource, item, path);
            }
            path.pop();
        }
        _ => {}
    }
    changed
}

/// A `runtime.RawExtension` — JSON its owner defines, not the API — is
/// never entered (#367, review finding 986d36e631b7): a DRA driver's
/// `opaque.parameters` (in a ResourceClaim's or ResourceClaimTemplate's
/// `devices.config[]`, and in a claim's `status.allocation`) and a claim's
/// `status.devices[].data`. Parameters `{"limits": {"connections": 10}}` were
/// stored as `{"connections": "10"}`, which the driver may no longer decode.
fn is_opaque(resource: &str, path: &[String], key: &str) -> bool {
    let parent = path.last().map(String::as_str);
    (key == "parameters" && parent == Some("opaque"))
        || (resource == "resourceclaims" && key == "data" && path.ends_with(&["status".to_string(), "devices".to_string(), ITEM.to_string()]))
}

fn is_quantity(resource: &str, path: &[String], key: &str) -> bool {
    let parent = path.last().map(String::as_str);
    let grandparent = path.len().checked_sub(2).map(|i| path[i].as_str());
    parent.is_some_and(|p| QUANTITY_MAPS.contains(&p))
        || QUANTITY_FIELDS.contains(&key)
        // CSIStorageCapacity's own `capacity` is one quantity, not a map.
        || (resource == "csistoragecapacities" && key == "capacity" && path.is_empty())
        // HPA metric targets and values (`target.value`, `current.value`).
        || (resource == "horizontalpodautoscalers"
            && key == "value"
            && matches!(parent, Some("target" | "current")))
        // DRA device capacity: `capacity.<name>.value`.
        || (key == "value" && grandparent == Some("capacity"))
}

/// A JSON number as upstream's canonical quantity string: DecimalSI, the
/// largest suffix of 10^3n that leaves an integer (`1` → `1`, `0.5` →
/// `500m`, `1000` → `1k`, `1500` → `1500`), below a nano rounded up to `1n`
/// as upstream rounds. A number too large to hold exactly is written as its
/// own text — still a string, which every client decodes.
pub fn canonical_number(n: &serde_json::Number) -> String {
    let text = n.to_string();
    canonical_decimal(&text).unwrap_or(text)
}

fn canonical_decimal(text: &str) -> Option<String> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (mantissa, exp_part) = match unsigned.find(['e', 'E']) {
        Some(i) => (&unsigned[..i], unsigned[i + 1..].parse::<i32>().ok()?),
        None => (unsigned, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{int}{frac}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some("0".into());
    }
    if digits.len() > 38 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut m: u128 = digits.parse().ok()?;
    let mut exp = exp_part.checked_sub(i32::try_from(frac.len()).ok()?)?;
    while m % 10 == 0 {
        m /= 10;
        exp += 1;
    }
    if exp < -9 {
        let shift = u32::try_from(-9 - exp).ok()?;
        m = match 10u128.checked_pow(shift) {
            Some(div) => m.div_ceil(div),
            None => 1,
        };
        exp = -9;
    }
    let e3 = (exp.div_euclid(3) * 3).min(18);
    m = m.checked_mul(10u128.checked_pow(u32::try_from(exp - e3).ok()?)?)?;
    let suffix = match e3 {
        -9 => "n",
        -6 => "u",
        -3 => "m",
        0 => "",
        3 => "k",
        6 => "M",
        9 => "G",
        12 => "T",
        15 => "P",
        _ => "E",
    };
    Some(format!("{}{m}{suffix}", if negative { "-" } else { "" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(v: serde_json::Value) -> String {
        match v {
            serde_json::Value::Number(n) => canonical_number(&n),
            other => panic!("{other} is not a number"),
        }
    }

    /// #367, review finding 986d36e631b7: a DRA driver's opaque payloads keep
    /// their numbers, even under keys that are quantity maps elsewhere, and a
    /// device request's `count` stays an integer; the claim's real quantity
    /// maps are still converted.
    #[test]
    fn opaque_driver_data_and_device_counts_are_left_as_written() {
        use serde_json::json;
        let params = json!({"limits": {"connections": 10}, "requests": {"cpu": 1}, "sizeLimit": 5, "value": 3});
        let config = json!([{"requests": ["r"], "opaque": {"driver": "gpu.example.com", "parameters": params}}]);
        let mut claim = json!({"spec": {"devices": {
                "requests": [{"name": "r", "deviceClassName": "gpu", "count": 2,
                              "exactly": {"deviceClassName": "gpu", "count": 2, "capacity": {"requests": {"memory": 1000}}}}],
                "config": config}},
            "status": {"allocation": {"devices": {"config": [{"source": "FromClaim", "requests": ["r"],
                    "opaque": {"driver": "gpu.example.com", "parameters": params}}]}},
                "devices": [{"driver": "gpu.example.com", "pool": "p", "device": "d", "data": {"limits": {"bw": 7}}}]}});
        let before = claim.clone();
        assert!(numbers_to_strings("resourceclaims", &mut claim), "the capacity request is a quantity");
        let r = &claim["spec"]["devices"]["requests"][0];
        assert_eq!(r["count"], 2);
        assert_eq!(r["exactly"]["count"], 2);
        assert_eq!(r["exactly"]["capacity"]["requests"]["memory"], "1k");
        assert_eq!(claim["spec"]["devices"]["config"], before["spec"]["devices"]["config"]);
        assert_eq!(claim["status"]["allocation"], before["status"]["allocation"]);
        assert_eq!(claim["status"]["devices"], before["status"]["devices"]);

        let mut template = json!({"spec": {"spec": {"devices": {"requests": [{"name": "r", "count": 1}], "config": config}}}});
        let before = template.clone();
        assert!(!numbers_to_strings("resourceclaimtemplates", &mut template));
        assert_eq!(template, before);

        // Array items of a real quantity path are still reached: a container's
        // requests, a LimitRange item's bounds.
        let mut pod = json!({"spec": {"containers": [{"name": "a", "resources": {"requests": {"cpu": 2}}}]}});
        assert!(numbers_to_strings("pods", &mut pod));
        assert_eq!(pod["spec"]["containers"][0]["resources"]["requests"]["cpu"], "2");
        let mut lr = json!({"spec": {"limits": [{"type": "Container", "max": {"cpu": 4}}]}});
        assert!(numbers_to_strings("limitranges", &mut lr));
        assert_eq!(lr["spec"]["limits"][0]["max"]["cpu"], "4");
    }

    /// #365: a canonicalized number reads back as the number it was, and a
    /// JSON number reads as its quoted form.
    #[test]
    fn canonical_forms_and_numbers_read_back() {
        use serde_json::json;
        assert_eq!(parse_cpu_millis("1k"), 1_000_000);
        assert_eq!(parse_cpu_millis("250m"), 250);
        assert_eq!(parse_cpu_millis("0.1"), 100);
        assert_eq!(parse_cpu_millis("2"), 2000);
        assert_eq!(parse_cpu_millis("x"), 0);
        assert_eq!(cpu_millis(&json!(2)), cpu_millis(&json!("2")));
        assert_eq!(cpu_millis(&json!(0.5)), 500);
        assert_eq!(base_units(&json!(1000)), base_units(&json!("1k")));
        assert_eq!(base_units(&json!("1Gi")), 1 << 30);
        assert_eq!(parse_quantity("1k"), Some(1000.0));
        assert_eq!(parse_quantity("pods"), None);
        for n in [json!(1), json!(0.5), json!(1000), json!(1500), json!(2.5)] {
            let s = canon(n.clone());
            assert_eq!(parse_cpu_millis(&s), cpu_millis(&n), "{n} as {s}");
        }
    }

    #[test]
    fn numbers_canonicalize_as_upstream() {
        use serde_json::json;
        assert_eq!(canon(json!(1)), "1");
        assert_eq!(canon(json!(0)), "0");
        assert_eq!(canon(json!(0.5)), "500m");
        assert_eq!(canon(json!(0.25)), "250m");
        assert_eq!(canon(json!(1.5)), "1500m");
        assert_eq!(canon(json!(1000)), "1k");
        assert_eq!(canon(json!(1500)), "1500");
        assert_eq!(canon(json!(2000000)), "2M");
        assert_eq!(canon(json!(1073741824u64)), "1073741824");
        assert_eq!(canon(json!(1e9)), "1G");
        assert_eq!(canon(json!(1e21)), "1000E");
        assert_eq!(canon(json!(0.0000000001)), "1n"); // rounded up, as upstream
        assert_eq!(canon(json!(-2)), "-2");
        // Every canonical form reads back as the number it came from.
        assert_eq!(parse_cpu_millis(&canon(json!(0.5))), 500);
        assert_eq!(parse_bytes(&canon(json!(2000000))), 2_000_000);
    }

    #[test]
    fn quantity_paths_become_strings_and_nothing_else() {
        use serde_json::json;
        let mut pod = json!({"spec": {
            "initContainers": [{"name": "install-cni-binaries",
                "resources": {"limits": {"cpu": 1, "memory": "1Gi"}, "requests": {"cpu": "100m"}}}],
            "containers": [{"name": "a", "ports": [{"containerPort": 80}],
                "resources": {"requests": {"memory": 1000}}}],
            "volumes": [{"name": "v", "emptyDir": {"sizeLimit": 1048576}}],
            "terminationGracePeriodSeconds": 30,
            "overhead": {"cpu": 0.25}},
            "status": {"containerStatuses": [{"restartCount": 3, "allocatedResources": {"cpu": 2}}]}});
        assert!(numbers_to_strings("pods", &mut pod));
        let c = &pod["spec"]["initContainers"][0]["resources"];
        assert_eq!(c["limits"], json!({"cpu": "1", "memory": "1Gi"}));
        assert_eq!(pod["spec"]["containers"][0]["resources"]["requests"]["memory"], "1k");
        assert_eq!(pod["spec"]["containers"][0]["ports"][0]["containerPort"], 80);
        assert_eq!(pod["spec"]["volumes"][0]["emptyDir"]["sizeLimit"], "1048576");
        assert_eq!(pod["spec"]["terminationGracePeriodSeconds"], 30);
        assert_eq!(pod["status"]["containerStatuses"][0]["restartCount"], 3);
        assert_eq!(pod["status"]["containerStatuses"][0]["allocatedResources"]["cpu"], "2");
        assert_eq!(pod["spec"]["overhead"]["cpu"], "250m");
        assert!(!numbers_to_strings("pods", &mut pod), "already strings: no change");

        let mut rc = json!({"overhead": {"podFixed": {"cpu": 0.25}}});
        assert!(numbers_to_strings("runtimeclasses", &mut rc));
        assert_eq!(rc["overhead"]["podFixed"]["cpu"], "250m");

        let mut csc = json!({"capacity": 1000000000, "maximumVolumeSize": 5});
        assert!(numbers_to_strings("csistoragecapacities", &mut csc));
        assert_eq!(csc, json!({"capacity": "1G", "maximumVolumeSize": "5"}));

        let mut hpa = json!({"spec": {"minReplicas": 1, "metrics": [{"type": "Pods",
            "pods": {"target": {"type": "AverageValue", "averageValue": 1, "averageUtilization": 50}}}]}});
        assert!(numbers_to_strings("horizontalpodautoscalers", &mut hpa));
        let t = &hpa["spec"]["metrics"][0]["pods"]["target"];
        assert_eq!(t["averageValue"], "1");
        assert_eq!(t["averageUtilization"], 50);
        assert_eq!(hpa["spec"]["minReplicas"], 1);

        // A CRD's schema keeps its numeric defaults and bounds; CRs are theirs.
        let mut crd = json!({"spec": {"versions": [{"schema": {"openAPIV3Schema": {
            "properties": {"x": {"type": "integer", "default": 3, "max": 4}}}}}]}});
        let before = crd.clone();
        assert!(!numbers_to_strings("customresourcedefinitions", &mut crd));
        assert!(!numbers_to_strings("virtualmachines.kubevirt.io", &mut crd));
        assert_eq!(crd, before);
    }

    #[test]
    fn binary_suffixes() {
        assert_eq!(parse_bytes("1Ki"), 1024);
        assert_eq!(parse_bytes("1Mi"), 1024 * 1024);
        assert_eq!(parse_bytes("1Gi"), 1024 * 1024 * 1024);
        assert_eq!(parse_bytes("2Ti"), 2 * 1024u64.pow(4));
    }

    #[test]
    fn decimal_suffixes_are_not_binary_ones() {
        assert_eq!(parse_bytes("1G"), 1_000_000_000);
        assert_eq!(parse_bytes("1k"), 1000);
        assert_ne!(parse_bytes("1G"), parse_bytes("1Gi"));
    }

    #[test]
    fn fractions_and_exponents() {
        assert_eq!(parse_bytes("1.5Gi"), 1_610_612_736);
        assert_eq!(parse_bytes("1e9"), 1_000_000_000);
        assert_eq!(parse_bytes("500"), 500);
    }

    #[test]
    fn nonsense_is_zero_not_a_panic() {
        assert_eq!(parse_bytes(""), 0);
        assert_eq!(parse_bytes("plenty"), 0);
        assert_eq!(parse_bytes("-5Gi"), 0);
    }

    #[test]
    fn cpu() {
        assert_eq!(parse_cpu_millis("100m"), 100);
        assert_eq!(parse_cpu_millis("1"), 1000);
        assert_eq!(parse_cpu_millis("0.5"), 500);
    }

    #[test]
    fn round_trips_to_the_largest_exact_suffix() {
        assert_eq!(format_bytes(1 << 30), "1Gi");
        assert_eq!(format_bytes(5 * (1 << 20)), "5Mi");
        assert_eq!(format_bytes(1000), "1000");
        assert_eq!(parse_bytes(&format_bytes(3 * (1 << 30))), 3 * (1 << 30));
    }
}
