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
