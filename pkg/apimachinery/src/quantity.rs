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
    let s = s.trim();
    if s.is_empty() {
        return 0;
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
            return scale(num, *mult);
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
            if num.parse::<f64>().is_ok() {
                return scale(num, *mult);
            }
        }
    }
    s.parse::<f64>().map(|v| v.max(0.0) as u64).unwrap_or(0)
}

fn scale(num: &str, mult: f64) -> u64 {
    match num.parse::<f64>() {
        Ok(v) if v > 0.0 => (v * mult) as u64,
        _ => 0,
    }
}

/// Parse a CPU quantity to millicores.
pub fn parse_cpu_millis(s: &str) -> u64 {
    let s = s.trim();
    if let Some(stripped) = s.strip_suffix('m') {
        stripped.parse::<f64>().map(|v| v.max(0.0) as u64).unwrap_or(0)
    } else {
        s.parse::<f64>().map(|v| (v.max(0.0) * 1000.0) as u64).unwrap_or(0)
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
                path.push(k.clone());
                changed |= walk(resource, child, path);
                path.pop();
            }
        }
        Value::Array(items) => {
            for item in items {
                changed |= walk(resource, item, path);
            }
        }
        _ => {}
    }
    changed
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
