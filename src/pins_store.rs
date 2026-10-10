//! Persistence for session pins so routing survives restarts.
//!
//! Pins are stored as JSON in the platform state directory:
//!   Windows:    %LOCALAPPDATA%\aiproxy\pins.json
//!   Linux/macOS: $XDG_CACHE_HOME/aiproxy/pins.json or ~/.cache/aiproxy/pins.json
//!
//! Writes are best-effort: a read-only or otherwise unwritable state directory
//! logs a warning (or, for permission errors, stays silent) instead of failing.

use crate::log::warn;
use crate::types::StoredPin;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const PIN_FILE_VERSION: u32 = 1;
const PINS_FILE: &str = "pins.json";

/// Platform state directory for aiproxy (fallback: ./.aiproxy-cache).
pub fn default_state_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(base) = env_path("LOCALAPPDATA") {
            return base.join("aiproxy");
        }
    } else {
        if let Some(xdg) = env_path("XDG_CACHE_HOME") {
            return xdg.join("aiproxy");
        }
        if let Some(home) = env_path("HOME") {
            return home.join(".cache").join("aiproxy");
        }
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(".aiproxy-cache"))
        .unwrap_or_else(|_| PathBuf::from(".aiproxy-cache"))
}

/// An environment variable that is set and non-empty (JavaScript truthiness).
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn pins_file(state_dir: &Path) -> PathBuf {
    state_dir.join(PINS_FILE)
}

/// Atomically write a JSON value to `<state_dir>/<filename>` (temp file +
/// rename). Silently no-ops when write permission is missing.
pub fn write_json_atomic(state_dir: &Path, filename: &str, data: &impl Serialize) {
    let result = (|| -> std::io::Result<()> {
        let payload = serde_json::to_string_pretty(data)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        std::fs::create_dir_all(state_dir)?;
        let file = state_dir.join(filename);
        let tmp = state_dir.join(format!("{filename}.tmp"));
        std::fs::write(&tmp, payload.as_bytes())?;
        match std::fs::rename(&tmp, &file) {
            Ok(()) => Ok(()),
            // rename over an existing file can fail on some platforms; fall back.
            Err(_) => std::fs::write(&file, payload.as_bytes()),
        }
    })();
    if let Err(err) = result {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            return; // persistence not enabled
        }
        warn(format!("could not persist {}: {}", filename, err));
    }
}

/// Read and parse a JSON file from the state dir, or None when missing/invalid.
pub fn read_json(state_dir: &Path, filename: &str) -> Option<Value> {
    let text = std::fs::read_to_string(state_dir.join(filename)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Load persisted pins. Returns an empty map when nothing is stored yet.
pub fn load_pins(state_dir: &Path) -> BTreeMap<String, StoredPin> {
    let Some(Value::Object(data)) = read_json(state_dir, PINS_FILE) else {
        return BTreeMap::new();
    };
    if data.get("version").and_then(Value::as_u64) != Some(u64::from(PIN_FILE_VERSION)) {
        return BTreeMap::new();
    }
    let Some(Value::Object(pins)) = data.get("pins") else {
        return BTreeMap::new();
    };
    pins.iter()
        .filter_map(|(key, value)| {
            serde_json::from_value::<StoredPin>(value.clone())
                .ok()
                .map(|pin| (key.clone(), pin))
        })
        .collect()
}

/// Persist pins atomically (temp file + rename). Best-effort.
pub fn save_pins(state_dir: &Path, pins: &BTreeMap<String, StoredPin>) {
    write_json_atomic(
        state_dir,
        PINS_FILE,
        &PinsFile {
            version: PIN_FILE_VERSION,
            pins,
        },
    );
}

#[derive(Serialize)]
struct PinsFile<'a> {
    version: u32,
    pins: &'a BTreeMap<String, StoredPin>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_round_trip_through_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut pins = BTreeMap::new();
        pins.insert(
            "f:abc123".to_string(),
            StoredPin {
                backend_id: "sandbox-1".to_string(),
                last_seen: 1_755_000_000_123,
            },
        );
        pins.insert(
            "h:my-session".to_string(),
            StoredPin {
                backend_id: "sandbox-2".to_string(),
                last_seen: 42,
            },
        );

        save_pins(dir.path(), &pins);
        let loaded = load_pins(dir.path());

        assert_eq!(loaded, pins);
    }

    #[test]
    fn persisted_pins_are_pretty_printed_with_version() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut pins = BTreeMap::new();
        pins.insert(
            "f:x".to_string(),
            StoredPin {
                backend_id: "b".to_string(),
                last_seen: 7,
            },
        );
        save_pins(dir.path(), &pins);

        let text = std::fs::read_to_string(pins_file(dir.path())).expect("read pins");
        let parsed: Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["pins"]["f:x"]["backendId"], "b");
        assert_eq!(parsed["pins"]["f:x"]["lastSeen"], 7);
        assert!(text.contains("\n  \""), "expected 2-space pretty printing");
    }

    #[test]
    fn load_pins_returns_empty_for_missing_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(load_pins(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn load_pins_returns_empty_for_malformed_json() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(pins_file(dir.path()), "not json").expect("write");
        assert!(load_pins(dir.path()).is_empty());
    }

    #[test]
    fn load_pins_returns_empty_for_wrong_version() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            pins_file(dir.path()),
            r#"{"version":2,"pins":{"f:x":{"backendId":"b","lastSeen":1}}}"#,
        )
        .expect("write");
        assert!(load_pins(dir.path()).is_empty());
    }

    #[test]
    fn load_pins_drops_invalid_entries() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            pins_file(dir.path()),
            r#"{"version":1,"pins":{
                "good":{"backendId":"b","lastSeen":1},
                "noLastSeen":{"backendId":"b"},
                "noBackend":{"lastSeen":1},
                "wrongTypes":{"backendId":5,"lastSeen":"x"}
            }}"#,
        )
        .expect("write");
        let loaded = load_pins(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["good"].backend_id, "b");
        assert_eq!(loaded["good"].last_seen, 1);
    }

    #[test]
    fn write_creates_missing_directories() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nested = dir.path().join("a").join("b").join("c");
        let mut pins = BTreeMap::new();
        pins.insert(
            "f:x".to_string(),
            StoredPin {
                backend_id: "b".to_string(),
                last_seen: 1,
            },
        );
        save_pins(&nested, &pins);
        assert_eq!(load_pins(&nested), pins);
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_json_atomic(dir.path(), "x.json", &serde_json::json!({"a": 1}));
        assert!(dir.path().join("x.json").exists());
        assert!(!dir.path().join("x.json.tmp").exists());
    }

    #[test]
    fn read_json_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(read_json(dir.path(), "nope.json").is_none());
        assert!(read_json(dir.path(), "nope.json").is_none());
    }

    #[test]
    fn default_state_dir_prefers_xdg_cache_home() {
        // These mutate the process environment, so keep them serialized.
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("XDG_CACHE_HOME");
        let prior_home = std::env::var_os("HOME");
        std::env::set_var("XDG_CACHE_HOME", "/xdg-cache");
        std::env::set_var("HOME", "/home/ted");
        assert_eq!(default_state_dir(), PathBuf::from("/xdg-cache/aiproxy"));
        restore("XDG_CACHE_HOME", prior);
        restore("HOME", prior_home);
    }

    #[test]
    fn default_state_dir_falls_back_to_home_cache() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("XDG_CACHE_HOME");
        let prior_home = std::env::var_os("HOME");
        std::env::remove_var("XDG_CACHE_HOME");
        std::env::set_var("HOME", "/home/ted");
        assert_eq!(
            default_state_dir(),
            PathBuf::from("/home/ted/.cache/aiproxy")
        );
        restore("XDG_CACHE_HOME", prior);
        restore("HOME", prior_home);
    }

    #[test]
    fn empty_env_vars_are_ignored() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("XDG_CACHE_HOME");
        let prior_home = std::env::var_os("HOME");
        std::env::set_var("XDG_CACHE_HOME", "");
        std::env::set_var("HOME", "/home/ted");
        // Empty XDG is falsy in JavaScript, so HOME wins.
        assert_eq!(
            default_state_dir(),
            PathBuf::from("/home/ted/.cache/aiproxy")
        );
        restore("XDG_CACHE_HOME", prior);
        restore("HOME", prior_home);
    }

    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn restore(name: &str, prior: Option<std::ffi::OsString>) {
        match prior {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
}
