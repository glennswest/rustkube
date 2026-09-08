//! `metav1.LabelSelector` matching — `matchLabels` **and** `matchExpressions`.
//!
//! The controllers each grew a matchLabels-only version of this, which is the
//! half that is easy: a selector carrying `matchExpressions` silently matched
//! everything the labels agreed on and ignored the expression, so a
//! PodDisruptionBudget written with `In`/`NotIn` guarded a different set of
//! pods than it named. Wrong quietly, in the direction of covering too much.

use serde_json::Value;

/// Does `labels` satisfy `selector`?
///
/// An empty selector (`{}`) matches everything, as upstream: it is the
/// "select all" spelling. A `null` selector matches nothing — the field was
/// not set, and the caller means "no selection", not "all of them".
pub fn matches(selector: &Value, labels: &Value) -> bool {
    if selector.is_null() {
        return false;
    }
    if !selector.is_object() {
        return false;
    }

    if let Some(ml) = selector.get("matchLabels").and_then(Value::as_object) {
        for (k, want) in ml {
            match labels.get(k) {
                Some(have) if have == want => {}
                _ => return false,
            }
        }
    }

    let exprs = match selector.get("matchExpressions").and_then(Value::as_array) {
        Some(e) => e,
        None => return true,
    };
    for expr in exprs {
        let key = expr["key"].as_str().unwrap_or("");
        let op = expr["operator"].as_str().unwrap_or("");
        let values: Vec<&str> = expr["values"]
            .as_array()
            .map(|vs| vs.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let have = labels.get(key).and_then(Value::as_str);
        let ok = match op {
            "In" => have.is_some_and(|v| values.contains(&v)),
            "NotIn" => have.is_none_or(|v| !values.contains(&v)),
            "Exists" => have.is_some(),
            "DoesNotExist" => have.is_none(),
            // An operator we do not know fails closed. Matching on a typo
            // selects objects nobody asked for, which is the dangerous way to
            // be wrong here.
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_selector_matches_everything_null_matches_nothing() {
        assert!(matches(&json!({}), &json!({"a": "b"})));
        assert!(!matches(&Value::Null, &json!({"a": "b"})));
    }

    #[test]
    fn match_labels_all_must_agree() {
        let sel = json!({"matchLabels": {"app": "web", "tier": "front"}});
        assert!(matches(&sel, &json!({"app": "web", "tier": "front", "x": "y"})));
        assert!(!matches(&sel, &json!({"app": "web"})));
    }

    #[test]
    fn match_expressions_are_honoured_not_ignored() {
        let sel = json!({"matchExpressions": [
            {"key": "env", "operator": "In", "values": ["prod", "stage"]}
        ]});
        assert!(matches(&sel, &json!({"env": "prod"})));
        assert!(!matches(&sel, &json!({"env": "dev"})));
        // The bug this replaced: no matchLabels meant "matches everything".
        assert!(!matches(&sel, &json!({})));
    }

    #[test]
    fn exists_notin_and_doesnotexist() {
        let exists = json!({"matchExpressions": [{"key": "k", "operator": "Exists"}]});
        assert!(matches(&exists, &json!({"k": "anything"})));
        assert!(!matches(&exists, &json!({})));

        let absent = json!({"matchExpressions": [{"key": "k", "operator": "DoesNotExist"}]});
        assert!(matches(&absent, &json!({})));
        assert!(!matches(&absent, &json!({"k": "v"})));

        let notin = json!({"matchExpressions": [
            {"key": "k", "operator": "NotIn", "values": ["a"]}
        ]});
        assert!(matches(&notin, &json!({"k": "b"})));
        assert!(matches(&notin, &json!({})), "an absent key is not in the set");
        assert!(!matches(&notin, &json!({"k": "a"})));
    }

    #[test]
    fn an_unknown_operator_fails_closed() {
        let sel = json!({"matchExpressions": [{"key": "k", "operator": "Iz", "values": ["a"]}]});
        assert!(!matches(&sel, &json!({"k": "a"})));
    }
}
