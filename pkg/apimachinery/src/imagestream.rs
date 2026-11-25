//! What an `image.openshift.io/v1` ImageStream says a tag points at (#386),
//! shared by the apiserver (a DeploymentConfig's or BuildConfig's
//! `instantiate`, a build's `ImageStreamTag` reference) and the
//! controller-manager's image trigger controller.
//!
//! Ported from openshift/library-go `pkg/image/imageutil` (read
//! 2026-11-25): `SplitImageStreamTag`, `LatestTaggedImage`,
//! `ResolveLatestTaggedImage` and `ResolveReferenceForTagEvent`.
//! ImageStreams are the registry's (#385): nothing here reads them, it only
//! interprets one.

use serde_json::Value;

/// The tag an image stream reference without one means.
pub const DEFAULT_TAG: &str = "latest";

/// `SplitImageStreamTag`: `<stream>:<tag>` into its two halves; the tag is
/// `latest` when none is given. `ok` is false when there was no tag.
pub fn split_tag(name: &str) -> (&str, &str, bool) {
    match name.split_once(':') {
        Some((stream, tag)) => (stream, tag, true),
        None => (name, DEFAULT_TAG, false),
    }
}

/// A `<stream>:<tag>` name as validation wants it: both halves present and
/// exactly one `:`.
pub fn valid_tag_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split(':').collect();
    parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty()
}

/// `LatestTaggedImage`: the newest event of `tag` in the stream's status.
pub fn latest_tag_event<'a>(stream: &'a Value, tag: &str) -> Option<&'a Value> {
    let tag = if tag.is_empty() { DEFAULT_TAG } else { tag };
    stream["status"]["tags"]
        .as_array()?
        .iter()
        .find(|t| t["tag"] == tag)?["items"]
        .as_array()?
        .first()
}

/// `ResolveLatestTaggedImage`: the pull spec `tag` of `stream` points at now
/// — the newest event's `dockerImageReference`, or, when the spec tag's
/// reference policy is `Local`, the stream's own repository by the image's
/// digest. `None` when the tag has no image yet.
pub fn resolve_latest_tagged_image(stream: &Value, tag: &str) -> Option<String> {
    let tag = if tag.is_empty() { DEFAULT_TAG } else { tag };
    let latest = latest_tag_event(stream, tag)?;
    let reference = latest["dockerImageReference"].as_str().unwrap_or("");
    let spec_tag = stream["spec"]["tags"].as_array().and_then(|t| t.iter().find(|t| t["name"] == tag));
    let local = spec_tag.is_some_and(|t| t["referencePolicy"]["type"] == "Local");
    let image = latest["image"].as_str().unwrap_or("");
    let repo = stream["status"]["dockerImageRepository"].as_str().unwrap_or("");
    let out = if local && !repo.is_empty() && !image.is_empty() {
        format!("{}@{image}", strip_tag_and_digest(repo))
    } else {
        reference.to_string()
    };
    (!out.is_empty()).then_some(out)
}

/// A repository pull spec without its `:tag` or `@digest`.
fn strip_tag_and_digest(spec: &str) -> &str {
    let spec = spec.split_once('@').map_or(spec, |(r, _)| r);
    let last_slash = spec.rfind('/').map_or(0, |i| i + 1);
    match spec[last_slash..].rfind(':') {
        Some(i) => &spec[..last_slash + i],
        None => spec,
    }
}

/// A container reference in a trigger's `fieldPath`, after the pod spec's
/// base path: `containers[?(@.name=="web")]`, `initContainers[0]`, …
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRef {
    pub init: bool,
    /// By name, or by index when `None`.
    pub name: Option<String>,
    pub index: usize,
    /// What follows the container: `image` for every trigger that does
    /// anything.
    pub rest: String,
}

/// `parseContainerReference` of openshift-controller-manager's annotation
/// trigger: `containers[…]` or `initContainers[…]`, where `…` is
/// `?(@.name=="x")`, `?(@.name="x")` (oc writes the first; both are read)
/// or an index, then `.rest`.
pub fn parse_container_path(path: &str) -> Option<ContainerRef> {
    let (init, after) = if let Some(a) = path.strip_prefix("containers[") {
        (false, a)
    } else if let Some(a) = path.strip_prefix("initContainers[") {
        (true, a)
    } else {
        return None;
    };
    let (name, index, rest) = if let Some(sel) = after.strip_prefix("?(@.name=") {
        let sel = sel.strip_prefix('=').unwrap_or(sel);
        let (quote, sel) = match sel.chars().next()? {
            q @ ('"' | '\'') => (q, &sel[1..]),
            _ => return None,
        };
        let end = sel.find(quote)?;
        let name = &sel[..end];
        let rest = sel[end + 1..].strip_prefix(")]")?;
        (Some(name.to_string()), 0, rest)
    } else {
        let end = after.find(']')?;
        let index: usize = after[..end].parse().ok()?;
        (None, index, &after[end + 1..])
    };
    let rest = rest.strip_prefix('.').unwrap_or(rest);
    Some(ContainerRef { init, name, index, rest: rest.to_string() })
}

/// The container a [`ContainerRef`] names in a pod spec.
pub fn container_mut<'a>(spec: &'a mut Value, r: &ContainerRef) -> Option<&'a mut Value> {
    let list = spec.get_mut(if r.init { "initContainers" } else { "containers" })?.as_array_mut()?;
    match &r.name {
        Some(name) => list.iter_mut().find(|c| c["name"] == name.as_str()),
        None => list.get_mut(r.index),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D0: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    fn stream(local: bool) -> Value {
        json!({
            "metadata": {"name": "web", "namespace": "p"},
            "spec": {"tags": [{"name": "latest", "referencePolicy": {"type": if local { "Local" } else { "Source" }}}]},
            "status": {
                "dockerImageRepository": "node1.lo:5000/p/web",
                "tags": [{"tag": "latest", "items": [
                    {"dockerImageReference": format!("quay.io/x/web@{D1}"), "image": D1},
                    {"dockerImageReference": format!("quay.io/x/web@{D0}"), "image": D0},
                ]}],
            },
        })
    }

    #[test]
    fn latest_event_and_reference_policy() {
        assert_eq!(resolve_latest_tagged_image(&stream(false), "latest").unwrap(), format!("quay.io/x/web@{D1}"));
        assert_eq!(resolve_latest_tagged_image(&stream(true), "").unwrap(), format!("node1.lo:5000/p/web@{D1}"));
        assert_eq!(resolve_latest_tagged_image(&stream(false), "v2"), None);
        let mut s = stream(false);
        s["status"]["tags"][0]["items"] = json!([]);
        assert_eq!(resolve_latest_tagged_image(&s, "latest"), None, "a tag with no image yet");
        assert_eq!(resolve_latest_tagged_image(&json!({"metadata": {}}), "latest"), None);
    }

    #[test]
    fn tag_names() {
        assert_eq!(split_tag("web:v1"), ("web", "v1", true));
        assert_eq!(split_tag("web"), ("web", "latest", false));
        assert!(valid_tag_name("web:v1"));
        for bad in ["web", ":v1", "web:", "a:b:c"] {
            assert!(!valid_tag_name(bad), "{bad}");
        }
        assert_eq!(strip_tag_and_digest("h:5000/p/web:v1"), "h:5000/p/web");
        assert_eq!(strip_tag_and_digest("h:5000/p/web"), "h:5000/p/web");
    }

    #[test]
    fn container_paths() {
        let r = parse_container_path("containers[?(@.name==\"web\")].image").unwrap();
        assert_eq!(r, ContainerRef { init: false, name: Some("web".into()), index: 0, rest: "image".into() });
        let r = parse_container_path("initContainers[?(@.name=\"setup\")].image").unwrap();
        assert!(r.init);
        assert_eq!(r.name.as_deref(), Some("setup"));
        let r = parse_container_path("containers[1].image").unwrap();
        assert_eq!((r.name, r.index, r.rest.as_str()), (None, 1, "image"));
        assert_eq!(parse_container_path("containers[?(@.name==\"web\")].env").unwrap().rest, "env");
        for bad in ["volumes[0].image", "containers[x].image", "containers[?(@.name==web)].image"] {
            assert!(parse_container_path(bad).is_none(), "{bad}");
        }
        let mut spec = json!({"containers": [{"name": "a", "image": "x"}, {"name": "web", "image": "y"}]});
        let r = parse_container_path("containers[?(@.name==\"web\")].image").unwrap();
        container_mut(&mut spec, &r).unwrap()["image"] = json!("z");
        assert_eq!(spec["containers"][1]["image"], "z");
    }
}
