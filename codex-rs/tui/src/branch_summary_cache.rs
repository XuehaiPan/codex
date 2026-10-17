//! Bounded, connection-scoped cache for remote HEAD discovery, not local refs or content snapshots.
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

pub(super) const REMOTE_DEFAULT_TTL: Duration = Duration::from_secs(600);

/// The connection namespace prevents equal paths on different workspace hosts sharing results.
/// Common-directory and remote URL changes naturally select a different entry.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct RemoteDefaultKey {
    pub(super) connection: Uuid,
    pub(super) common_dir: String,
    pub(super) remote: String,
    pub(super) url: String,
}

#[derive(Default)]
pub(super) struct RemoteDefaultCache {
    entries: HashMap<RemoteDefaultKey, (Instant, Option<String>)>,
}

impl RemoteDefaultCache {
    pub(super) fn get(&self, key: &RemoteDefaultKey, now: Instant) -> Option<Option<String>> {
        self.entries
            .get(key)
            .filter(|(expires, _)| now < *expires)
            .map(|(_, value)| value.clone())
    }

    pub(super) fn insert(&mut self, key: RemoteDefaultKey, value: Option<String>, now: Instant) {
        self.entries.retain(|_, (expires, _)| now < *expires);
        // Discovery is an optimization. Dropping this small cache is preferable to retaining every
        // repository/connection ever visited by a long-lived TUI process.
        if self.entries.len() >= 128 {
            self.entries.clear();
        }
        self.entries.insert(key, (now + REMOTE_DEFAULT_TTL, value));
    }
}

pub(super) static REMOTE_DEFAULTS: LazyLock<Mutex<RemoteDefaultCache>> =
    LazyLock::new(Mutex::default);

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn branch_changes_remote_discovery_cache_expires_and_separates_contexts() {
        let mut cache = RemoteDefaultCache::default();
        let now = Instant::now();
        let key = RemoteDefaultKey {
            connection: Uuid::new_v4(),
            common_dir: "/repo/.git".into(),
            remote: "origin".into(),
            url: "https://example.invalid/repo".into(),
        };
        cache.insert(key.clone(), Some("main".into()), now);
        assert_eq!(
            cache.get(&key, now + REMOTE_DEFAULT_TTL - Duration::from_nanos(1)),
            Some(Some("main".into()))
        );
        assert_eq!(cache.get(&key, now + REMOTE_DEFAULT_TTL), None);
        let mut other = key.clone();
        other.connection = Uuid::new_v4();
        assert_eq!(cache.get(&other, now), None);
        other = key.clone();
        other.common_dir = "/another/.git".into();
        assert_eq!(cache.get(&other, now), None);
        other = key.clone();
        other.url = "https://example.invalid/other".into();
        assert_eq!(cache.get(&other, now), None);
        cache.insert(key.clone(), None, now);
        assert_eq!(cache.get(&key, now), Some(None));
        assert_eq!(cache.get(&key, now + REMOTE_DEFAULT_TTL), None);
    }
}
