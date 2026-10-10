//! Persistence for backend request stats and the recent-routes log so a
//! restart restores the dashboard to its previous state.

use crate::pins_store::{read_json, write_json_atomic};
use crate::types::RoutingEntry;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

const STATS_FILE_VERSION: u32 = 1;
const STATS_FILE: &str = "stats.json";

/// Restored dashboard state.
pub struct StatsSnapshot {
    pub backend_counts: BTreeMap<String, u64>,
    pub recent: Vec<RoutingEntry>,
}

impl StatsSnapshot {
    pub fn empty() -> Self {
        Self {
            backend_counts: BTreeMap::new(),
            recent: Vec::new(),
        }
    }
}

/// Load persisted stats. Returns empty stats when nothing is stored yet.
pub fn load_stats(state_dir: &Path) -> StatsSnapshot {
    let Some(Value::Object(data)) = read_json(state_dir, STATS_FILE) else {
        return StatsSnapshot::empty();
    };
    if data.get("version").and_then(Value::as_u64) != Some(u64::from(STATS_FILE_VERSION)) {
        return StatsSnapshot::empty();
    }

    let mut backend_counts = BTreeMap::new();
    if let Some(Value::Object(counts)) = data.get("backendCounts") {
        for (id, n) in counts {
            if let Some(n) = n.as_u64() {
                backend_counts.insert(id.clone(), n);
            }
        }
    }

    let recent = match data.get("recent") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| serde_json::from_value::<RoutingEntry>(item.clone()).ok())
            .collect(),
        _ => Vec::new(),
    };

    StatsSnapshot {
        backend_counts,
        recent,
    }
}

/// Persist stats atomically. Best-effort.
pub fn save_stats(
    state_dir: &Path,
    backend_counts: &BTreeMap<String, u64>,
    recent: &[RoutingEntry],
) {
    write_json_atomic(
        state_dir,
        STATS_FILE,
        &StatsFile {
            version: STATS_FILE_VERSION,
            backend_counts,
            recent,
        },
    );
}

#[derive(Serialize)]
struct StatsFile<'a> {
    version: u32,
    #[serde(rename = "backendCounts")]
    backend_counts: &'a BTreeMap<String, u64>,
    recent: &'a [RoutingEntry],
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> RoutingEntry {
        RoutingEntry {
            at: "2026-08-09T12:00:00.000Z".to_string(),
            session_id: "f:abc123".to_string(),
            model: "sandbox-gpt-4o".to_string(),
            backend_id: "sandbox-1".to_string(),
            endpoint: "http://127.0.0.1:6061/v1".to_string(),
            status: 200,
            ms: 42,
        }
    }

    #[test]
    fn stats_round_trip_through_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let counts = BTreeMap::from([("sandbox-1".to_string(), 5), ("sandbox-2".to_string(), 3)]);
        let recent = vec![entry()];

        save_stats(dir.path(), &counts, &recent);
        let loaded = load_stats(dir.path());

        assert_eq!(loaded.backend_counts, counts);
        assert_eq!(loaded.recent.len(), 1);
        assert_eq!(loaded.recent[0], entry());
    }

    #[test]
    fn load_stats_returns_empty_for_missing_or_malformed_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let empty = load_stats(dir.path());
        assert!(empty.backend_counts.is_empty());
        assert!(empty.recent.is_empty());

        std::fs::write(dir.path().join(STATS_FILE), "not json").expect("write");
        let malformed = load_stats(dir.path());
        assert!(malformed.backend_counts.is_empty());
        assert!(malformed.recent.is_empty());
    }

    #[test]
    fn load_stats_drops_invalid_entries_and_counts() {
        let dir = tempfile::tempdir().expect("temp dir");
        let bad = serde_json::json!({
            "version": 1,
            "backendCounts": {"x": "oops", "y": -1, "z": 2},
            "recent": [{"nope": true}]
        });
        std::fs::write(
            dir.path().join(STATS_FILE),
            serde_json::to_string(&bad).expect("json"),
        )
        .expect("write");

        let loaded = load_stats(dir.path());
        assert_eq!(
            loaded.backend_counts,
            BTreeMap::from([("z".to_string(), 2)])
        );
        assert!(loaded.recent.is_empty());
    }

    #[test]
    fn load_stats_rejects_wrong_version() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join(STATS_FILE),
            r#"{"version":2,"backendCounts":{},"recent":[]}"#,
        )
        .expect("write");
        let loaded = load_stats(dir.path());
        assert!(loaded.backend_counts.is_empty());
        assert!(loaded.recent.is_empty());
    }

    #[test]
    fn load_stats_drops_entries_with_wrong_field_types() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut bad = serde_json::to_value(entry()).expect("json");
        bad["status"] = serde_json::json!("200");
        let missing = serde_json::json!({});
        let payload = serde_json::json!({
            "version": 1,
            "backendCounts": {},
            "recent": [bad, missing]
        });
        std::fs::write(
            dir.path().join(STATS_FILE),
            serde_json::to_string(&payload).expect("json"),
        )
        .expect("write");

        assert!(load_stats(dir.path()).recent.is_empty());
    }

    #[test]
    fn recent_entries_use_camel_case_field_names() {
        let text = serde_json::to_string(&entry()).expect("json");
        assert!(text.contains("\"sessionId\""));
        assert!(text.contains("\"backendId\""));
    }
}
