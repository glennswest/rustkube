//! Label and field selector parsing and evaluation.
//!
//! Supports standard K8s selector syntax:
//! - Label: `key=value`, `key!=value`, `key in (a,b)`, `key notin (a,b)`, `key`, `!key`
//! - Field: `metadata.name=foo`, `metadata.namespace=bar`, `spec.nodeName=node1`

use serde_json::Value;

/// A single label selector requirement.
#[derive(Debug, Clone)]
pub enum LabelRequirement {
    Eq(String, String),
    NotEq(String, String),
    In(String, Vec<String>),
    NotIn(String, Vec<String>),
    Exists(String),
    DoesNotExist(String),
}

/// A single field selector requirement.
#[derive(Debug, Clone)]
pub struct FieldRequirement {
    pub field: String,
    pub value: String,
    pub negate: bool,
}

/// Parse a label selector string into requirements.
pub fn parse_label_selector(s: &str) -> Vec<LabelRequirement> {
    let s = s.trim();
    if s.is_empty() {
        return Vec::new();
    }
    let mut reqs = Vec::new();
    for part in split_selector(s) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(req) = parse_one_label(part) {
            reqs.push(req);
        }
    }
    reqs
}

/// Parse a field selector string into requirements.
pub fn parse_field_selector(s: &str) -> Vec<FieldRequirement> {
    let s = s.trim();
    if s.is_empty() {
        return Vec::new();
    }
    let mut reqs = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(idx) = part.find("!=") {
            reqs.push(FieldRequirement {
                field: part[..idx].trim().to_string(),
                value: part[idx + 2..].trim().to_string(),
                negate: true,
            });
        } else if let Some(idx) = part.find('=') {
            reqs.push(FieldRequirement {
                field: part[..idx].trim().to_string(),
                value: part[idx + 1..].trim().to_string(),
                negate: false,
            });
        }
    }
    reqs
}

/// Check if a label map matches all label requirements.
pub fn matches_label_selector(labels: &serde_json::Map<String, Value>, reqs: &[LabelRequirement]) -> bool {
    reqs.iter().all(|req| match req {
        LabelRequirement::Eq(k, v) => labels.get(k).and_then(|x| x.as_str()) == Some(v.as_str()),
        LabelRequirement::NotEq(k, v) => labels.get(k).and_then(|x| x.as_str()) != Some(v.as_str()),
        LabelRequirement::In(k, vals) => {
            labels.get(k).and_then(|x| x.as_str()).map(|v| vals.iter().any(|i| i == v)).unwrap_or(false)
        }
        LabelRequirement::NotIn(k, vals) => {
            labels.get(k).and_then(|x| x.as_str()).map(|v| !vals.iter().any(|i| i == v)).unwrap_or(true)
        }
        LabelRequirement::Exists(k) => labels.contains_key(k),
        LabelRequirement::DoesNotExist(k) => !labels.contains_key(k),
    })
}

/// Check if an object matches all field requirements.
pub fn matches_field_selector(obj: &Value, reqs: &[FieldRequirement]) -> bool {
    reqs.iter().all(|req| {
        let actual = resolve_field(obj, &req.field);
        if req.negate {
            actual.as_deref() != Some(req.value.as_str())
        } else {
            actual.as_deref() == Some(req.value.as_str())
        }
    })
}

/// Filter a list of objects by both label and field selectors.
pub fn filter_objects(
    items: Vec<Value>,
    label_sel: &Option<String>,
    field_sel: &Option<String>,
) -> Vec<Value> {
    let label_reqs = label_sel.as_deref().map(parse_label_selector).unwrap_or_default();
    let field_reqs = field_sel.as_deref().map(parse_field_selector).unwrap_or_default();

    if label_reqs.is_empty() && field_reqs.is_empty() {
        return items;
    }

    items
        .into_iter()
        .filter(|obj| {
            let label_match = if label_reqs.is_empty() {
                true
            } else {
                let labels = obj["metadata"]["labels"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                matches_label_selector(&labels, &label_reqs)
            };
            let field_match = if field_reqs.is_empty() {
                true
            } else {
                matches_field_selector(obj, &field_reqs)
            };
            label_match && field_match
        })
        .collect()
}

/// Check if a single watch event object matches selectors.
pub fn matches_selectors(
    obj: &Value,
    label_sel: &Option<String>,
    field_sel: &Option<String>,
) -> bool {
    let label_reqs = label_sel.as_deref().map(parse_label_selector).unwrap_or_default();
    let field_reqs = field_sel.as_deref().map(parse_field_selector).unwrap_or_default();

    if !label_reqs.is_empty() {
        let labels = obj["metadata"]["labels"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        if !matches_label_selector(&labels, &label_reqs) {
            return false;
        }
    }
    if !field_reqs.is_empty() && !matches_field_selector(obj, &field_reqs) {
        return false;
    }
    true
}

// --- internal helpers ---

/// Split a selector on commas, but not inside parentheses.
fn split_selector(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Parse a single label requirement expression.
fn parse_one_label(s: &str) -> Option<LabelRequirement> {
    // "key notin (a,b,c)"
    if let Some(idx) = s.find(" notin ") {
        let key = s[..idx].trim().to_string();
        let vals = parse_set_values(&s[idx + 7..]);
        return Some(LabelRequirement::NotIn(key, vals));
    }
    // "key in (a,b,c)"
    if let Some(idx) = s.find(" in ") {
        let key = s[..idx].trim().to_string();
        let vals = parse_set_values(&s[idx + 4..]);
        return Some(LabelRequirement::In(key, vals));
    }
    // "key!=value"
    if let Some(idx) = s.find("!=") {
        let key = s[..idx].trim().to_string();
        let val = s[idx + 2..].trim().to_string();
        return Some(LabelRequirement::NotEq(key, val));
    }
    // "key=value" or "key==value"
    if let Some(idx) = s.find('=') {
        let key = s[..idx].trim().to_string();
        let rest = &s[idx + 1..];
        let val = rest.strip_prefix('=').unwrap_or(rest).trim().to_string();
        return Some(LabelRequirement::Eq(key, val));
    }
    // "!key" — does not exist
    if let Some(rest) = s.strip_prefix('!') {
        let key = rest.trim().to_string();
        if !key.is_empty() {
            return Some(LabelRequirement::DoesNotExist(key));
        }
    }
    // "key" — exists
    let key = s.trim().to_string();
    if !key.is_empty() {
        return Some(LabelRequirement::Exists(key));
    }
    None
}

/// Parse "(a, b, c)" into vec of strings.
fn parse_set_values(s: &str) -> Vec<String> {
    let s = s.trim();
    let s = s.strip_prefix('(').unwrap_or(s);
    let s = s.strip_suffix(')').unwrap_or(s);
    s.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect()
}

/// Resolve a dotted field path (e.g. "metadata.name") in a JSON value.
fn resolve_field(obj: &Value, path: &str) -> Option<String> {
    let mut current = obj;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    match current {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => None,
        _ => Some(current.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_label_eq() {
        let reqs = parse_label_selector("app=nginx");
        let labels = serde_json::from_value::<serde_json::Map<String, Value>>(
            json!({"app": "nginx", "tier": "frontend"}),
        )
        .unwrap();
        assert!(matches_label_selector(&labels, &reqs));
    }

    #[test]
    fn test_label_neq() {
        let reqs = parse_label_selector("app!=apache");
        let labels = serde_json::from_value::<serde_json::Map<String, Value>>(
            json!({"app": "nginx"}),
        )
        .unwrap();
        assert!(matches_label_selector(&labels, &reqs));
    }

    #[test]
    fn test_label_in() {
        let reqs = parse_label_selector("env in (prod,staging)");
        let labels = serde_json::from_value::<serde_json::Map<String, Value>>(
            json!({"env": "prod"}),
        )
        .unwrap();
        assert!(matches_label_selector(&labels, &reqs));
    }

    #[test]
    fn test_label_exists() {
        let reqs = parse_label_selector("app");
        let labels = serde_json::from_value::<serde_json::Map<String, Value>>(
            json!({"app": "nginx"}),
        )
        .unwrap();
        assert!(matches_label_selector(&labels, &reqs));
    }

    #[test]
    fn test_field_selector() {
        let reqs = parse_field_selector("metadata.name=my-pod");
        let obj = json!({"metadata": {"name": "my-pod"}, "spec": {}});
        assert!(matches_field_selector(&obj, &reqs));
    }

    #[test]
    fn test_filter_objects() {
        let items = vec![
            json!({"metadata": {"name": "a", "labels": {"app": "nginx"}}}),
            json!({"metadata": {"name": "b", "labels": {"app": "apache"}}}),
            json!({"metadata": {"name": "c", "labels": {"app": "nginx"}}}),
        ];
        let filtered = filter_objects(items, &Some("app=nginx".into()), &None);
        assert_eq!(filtered.len(), 2);
    }
}
