use serde::{Deserialize, Serialize};

/// A watch event, mirroring the Kubernetes watch event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WatchEvent {
    Added {
        key: String,
        value: Vec<u8>,
        revision: u64,
    },
    Modified {
        key: String,
        value: Vec<u8>,
        revision: u64,
    },
    Deleted {
        key: String,
        revision: u64,
        /// The object as it was before the delete, when something knows it.
        ///
        /// The store's own watch does not carry it; the apiserver's watch
        /// cache does, from the snapshot it keeps, and fills it in before the
        /// event reaches a client (#100). A DELETED event is meant to carry
        /// the object's last state — a finalizer controller or an event feed
        /// reads fields of what went away — and a name-only tombstone is the
        /// fallback for when nobody held it.
        #[serde(default)]
        prev_value: Option<Vec<u8>>,
    },
    Bookmark {
        revision: u64,
    },
}

impl WatchEvent {
    /// The revision associated with this event.
    pub fn revision(&self) -> u64 {
        match self {
            Self::Added { revision, .. } => *revision,
            Self::Modified { revision, .. } => *revision,
            Self::Deleted { revision, .. } => *revision,
            Self::Bookmark { revision } => *revision,
        }
    }
}
