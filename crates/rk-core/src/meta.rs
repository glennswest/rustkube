/// Internal object metadata extensions.
///
/// The API server stores the Kubernetes `resourceVersion` as a stringified
/// u64 revision from the KV store. This module provides helpers for
/// converting between the two.

/// Convert a store revision to a Kubernetes resourceVersion string.
pub fn revision_to_resource_version(revision: u64) -> String {
    revision.to_string()
}

/// Parse a Kubernetes resourceVersion string back to a store revision.
pub fn resource_version_to_revision(rv: &str) -> Option<u64> {
    rv.parse().ok()
}
