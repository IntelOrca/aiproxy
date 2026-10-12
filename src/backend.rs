//! Backend pool: health tracking, weighted/priority selection and sticky
//! session pins.
//!
//! The Deno version was a class mutated from the single-threaded request
//! handler. Here every request runs on its own task, so the mutable state
//! (healthy flags and the pin map) lives behind a short-held `Mutex`; no
//! await ever happens while it is locked, and streaming bodies never touch it.

use crate::isotime::now_ms;
use crate::log::info;
use crate::pins_store::{default_state_dir, load_pins, save_pins};
use crate::types::{BackendConfig, Config, StoredPin};
use rand::Rng;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Drop pins once the map grows past this many entries.
const PIN_EVICTION_THRESHOLD: usize = 10_000;
/// ...down to this many.
const PIN_EVICTION_TARGET: usize = 8_000;
/// How often pending pin writes are flushed to disk.
const PERSIST_INTERVAL_MS: u64 = 500;

/// Append a path to an OpenAI-compatible API root, preserving any base prefix.
pub fn with_path(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

/// A backend chosen for a request (a clone of the configured backend).
pub type BackendChoice = BackendConfig;

/// One row of the backend health report served to the dashboard.
#[derive(Clone, Debug)]
pub struct BackendReport {
    pub id: String,
    pub base_url: String,
    pub healthy: bool,
    pub priority: Option<i64>,
}

struct BackendState {
    config: BackendConfig,
    healthy: bool,
}

#[derive(Default)]
struct ManagerInner {
    states: Vec<BackendState>,
    pins: BTreeMap<String, StoredPin>,
}

pub struct BackendManager {
    inner: Mutex<ManagerInner>,
    config: Config,
    state_dir: PathBuf,
    dirty: AtomicBool,
}

/// How a pick resolves the session's pin.
enum Selection {
    /// Serve the pinned backend, refreshing its `lastSeen`.
    Pinned(String),
    /// The pinned backend is healthy but excluded for this request (a
    /// rate-limit failover): pick another backend WITHOUT touching the pin.
    Failover,
    /// No usable pin: pick fresh and pin the session to the result.
    Pin,
}

impl BackendManager {
    pub fn new(config: Config) -> Self {
        let state_dir = config
            .state_dir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(default_state_dir);
        let states = config
            .backends
            .iter()
            .map(|b| BackendState {
                config: b.clone(),
                healthy: true,
            })
            .collect();
        let mut manager = Self {
            inner: Mutex::new(ManagerInner {
                states,
                pins: BTreeMap::new(),
            }),
            config,
            state_dir,
            dirty: AtomicBool::new(false),
        };
        manager.restore_pins();
        manager
    }

    /// The directory pins are persisted to.
    pub fn state_dir(&self) -> &PathBuf {
        &self.state_dir
    }

    /// Load persisted pins for backends that still exist, dropping expired ones.
    fn restore_pins(&mut self) {
        let valid_ids: HashSet<&str> = self.config.backends.iter().map(|b| b.id.as_str()).collect();
        let now = now_ms();
        let mut restored = 0;
        for (key, pin) in load_pins(&self.state_dir) {
            if !valid_ids.contains(pin.backend_id.as_str()) {
                continue;
            }
            if now.saturating_sub(pin.last_seen) > self.config.session_ttl_ms {
                continue;
            }
            self.lock().pins.insert(key, pin);
            restored += 1;
        }
        if restored > 0 {
            info(format!(
                "restored {} session pin(s) from {}",
                restored,
                self.state_dir.display()
            ));
        }
    }

    /// Schedule a persisted write of the current pins (debounced).
    fn persist_soon(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Write pins to disk if anything changed.
    pub fn flush(&self) {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let pins = self.lock().pins.clone();
        save_pins(&self.state_dir, &pins);
    }

    /// Stop health checks and persistence, flushing pending pin writes.
    pub fn stop(&self) {
        self.flush();
    }

    /// Flush pending pins every 500ms until the task is aborted.
    pub async fn run_persistence(self: std::sync::Arc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_millis(PERSIST_INTERVAL_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            self.flush();
        }
    }

    /// Spawn the debounced pin-persistence task.
    pub fn start_persistence(self: &std::sync::Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(std::sync::Arc::clone(self).run_persistence())
    }

    /// Probe every backend's /models endpoint; any response (even an error
    /// status) means it is reachable.
    pub async fn check_all(&self, client: &reqwest::Client) {
        let timeout = Duration::from_millis(self.config.health_check_timeout_ms);
        let targets: Vec<(String, String)> = self
            .lock()
            .states
            .iter()
            .map(|s| {
                (
                    s.config.id.clone(),
                    with_path(&s.config.base_url, "/models"),
                )
            })
            .collect();

        let mut probes = tokio::task::JoinSet::new();
        for (id, url) in targets {
            let client = client.clone();
            probes.spawn(async move { (id, probe(&client, &url, timeout).await) });
        }

        let mut healthy = HashMap::with_capacity(self.backend_count());
        while let Some(joined) = probes.join_next().await {
            if let Ok((id, ok)) = joined {
                healthy.insert(id, ok);
            }
        }

        let mut inner = self.lock();
        for state in &mut inner.states {
            if let Some(ok) = healthy.get(&state.config.id) {
                state.healthy = *ok;
            }
        }
    }

    /// Probe backends now and on every `healthCheckIntervalMs` tick.
    pub async fn run_health_checks(self: std::sync::Arc<Self>, client: reqwest::Client) {
        loop {
            self.check_all(&client).await;
            tokio::time::sleep(Duration::from_millis(self.config.health_check_interval_ms)).await;
        }
    }

    /// Spawn the periodic health-check task.
    pub fn start_health_checks(
        self: &std::sync::Arc<Self>,
        client: reqwest::Client,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(std::sync::Arc::clone(self).run_health_checks(client))
    }

    pub fn backend_count(&self) -> usize {
        self.lock().states.len()
    }

    pub fn health_report(&self) -> Vec<BackendReport> {
        self.lock()
            .states
            .iter()
            .map(|s| BackendReport {
                id: s.config.id.clone(),
                base_url: s.config.base_url.clone(),
                healthy: s.healthy,
                priority: s.config.priority,
            })
            .collect()
    }

    /// The backend a session is pinned to, if any.
    pub fn pin_backend_id(&self, session_key: &str) -> Option<String> {
        self.lock()
            .pins
            .get(session_key)
            .map(|p| p.backend_id.clone())
    }

    pub fn pin_count(&self) -> usize {
        self.lock().pins.len()
    }

    /// Pick a backend.
    ///
    /// With `sticky` enabled and a session key, returns the backend this
    /// conversation was pinned to (if still healthy and not excluded). If the
    /// pinned backend is healthy but excluded (rate-limit failover in
    /// progress), picks another backend WITHOUT touching the pin, so the
    /// conversation keeps its preferred backend for next time. If the pinned
    /// backend is unhealthy, the pin is dropped and a fresh backend is picked.
    /// The pick itself never pins: upstream pins the session once a request
    /// actually succeeds, so failed responses cannot strand a session on a
    /// backend that cannot serve it.
    ///
    /// New picks prefer the lowest `priority` group of healthy backends and do
    /// a weighted random pick within it.
    pub fn pick(
        &self,
        session_key: Option<&str>,
        exclude: &HashSet<String>,
    ) -> Option<BackendChoice> {
        let mut inner = self.lock();

        let pool: Vec<BackendConfig> = inner
            .states
            .iter()
            .filter(|s| s.healthy && !exclude.contains(&s.config.id))
            .map(|s| s.config.clone())
            .collect();
        if pool.is_empty() {
            return None;
        }

        let pinned_id = if self.config.sticky {
            session_key.and_then(|key| inner.pins.get(key).map(|p| p.backend_id.clone()))
        } else {
            None
        };

        let (selection, changed) = match (pinned_id, session_key) {
            (Some(pinned_id), Some(key)) => {
                if pool.iter().any(|s| s.id == pinned_id) {
                    // Preferred backend is healthy and not excluded — stick
                    // with it, refreshing its expiry.
                    if let Some(pin) = inner.pins.get_mut(key) {
                        pin.last_seen = now_ms();
                    }
                    (Selection::Pinned(pinned_id), true)
                } else if inner
                    .states
                    .iter()
                    .any(|s| s.config.id == pinned_id && s.healthy)
                {
                    // Excluded for this request (e.g. rate-limited) — use
                    // another backend without touching the pin. upstream pins
                    // the session to whichever backend serves a request.
                    (Selection::Failover, false)
                } else {
                    // Pinned backend is down — drop the pin and pick fresh.
                    inner.pins.remove(key);
                    (Selection::Pin, true)
                }
            }
            // No pin for this session yet (or sticky routing is off).
            _ => (Selection::Pin, false),
        };

        let chosen = match selection {
            Selection::Pinned(id) => pool.into_iter().find(|s| s.id == id),
            Selection::Failover => {
                // Rate-limit failover: the pin is deliberately left alone so
                // the conversation returns to its preferred backend next turn.
                Some(weighted_pick(&pool))
            }
            Selection::Pin => Some(weighted_pick(&pool)),
        };

        drop(inner);
        if changed {
            self.persist_soon();
        }
        chosen
    }

    /// Re-pin a session to a specific backend (upstream calls this once a
    /// request has actually been served successfully).
    pub fn pin_to(&self, session_key: &str, backend_id: &str) {
        if !self.config.sticky {
            return;
        }
        {
            let mut inner = self.lock();
            inner.pins.insert(
                session_key.to_string(),
                StoredPin {
                    backend_id: backend_id.to_string(),
                    last_seen: now_ms(),
                },
            );
            let config_ttl = self.config.session_ttl_ms;
            evict_pins(&mut inner, config_ttl);
        }
        self.persist_soon();
    }

    /// Whether a backend is currently marked healthy.
    pub fn is_healthy(&self, backend_id: &str) -> bool {
        self.lock()
            .states
            .iter()
            .any(|s| s.config.id == backend_id && s.healthy)
    }

    /// Mark a backend unhealthy after a failed forward.
    pub fn mark_down(&self, backend_id: &str) {
        let mut inner = self.lock();
        if let Some(state) = inner.states.iter_mut().find(|s| s.config.id == backend_id) {
            state.healthy = false;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ManagerInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One HTTP probe; Ok means the backend answered.
async fn probe(client: &reqwest::Client, url: &str, timeout: Duration) -> bool {
    client.get(url).timeout(timeout).send().await.is_ok()
}

/// Prefer the lowest-priority group (backends without a priority are equal and
/// rank below any explicitly prioritized backend), then pick within it by
/// weight.
fn weighted_pick(pool: &[BackendConfig]) -> BackendChoice {
    let min_priority = pool
        .iter()
        .map(BackendConfig::effective_priority)
        .min()
        .unwrap_or(i64::MAX);
    let candidates: Vec<&BackendConfig> = pool
        .iter()
        .filter(|s| s.effective_priority() == min_priority)
        .collect();

    let total: f64 = candidates.iter().map(|s| s.weight).sum();
    if total <= 0.0 {
        // Degenerate weights (all zero or negative): fall back to the first
        // candidate, as the Deno accumulator did when `r <= 0` immediately.
        return candidates[0].clone();
    }

    let mut r = rand::thread_rng().gen_range(0.0..total);
    let mut chosen = candidates[0].clone();
    for candidate in candidates {
        r -= candidate.weight;
        if r <= 0.0 {
            chosen = candidate.clone();
            break;
        }
    }
    chosen
}

/// Re-pin a session to a specific backend (e.g. after a retry/failover).
fn evict_pins(inner: &mut ManagerInner, ttl_ms: u64) {
    if inner.pins.len() < PIN_EVICTION_THRESHOLD {
        return;
    }
    let now = now_ms();
    let expired: Vec<String> = inner
        .pins
        .iter()
        .filter(|(_, pin)| now.saturating_sub(pin.last_seen) > ttl_ms)
        .map(|(key, _)| key.clone())
        .collect();
    for key in expired {
        inner.pins.remove(&key);
        if inner.pins.len() < PIN_EVICTION_TARGET {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config plus the temp dir it persists to (kept alive by the caller).
    fn test_config(backends: Vec<BackendConfig>, sticky: bool) -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            port: 0,
            backends,
            sticky,
            health_check_interval_ms: 60_000,
            health_check_timeout_ms: 1_000,
            session_ttl_ms: 60_000,
            state_dir: Some(dir.path().display().to_string()),
            ..Default::default()
        };
        (config, dir)
    }

    fn backends(ids: &[(&str, i64)]) -> Vec<BackendConfig> {
        ids.iter()
            .enumerate()
            .map(|(i, (id, priority))| BackendConfig {
                id: (*id).to_string(),
                base_url: format!("http://127.0.0.1:{}/v1", 1000 + i),
                weight: 1.0,
                model: None,
                api_key: None,
                priority: if *priority < 0 { None } else { Some(*priority) },
                retries: 0,
            })
            .collect()
    }

    fn exclusion(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sticky_same_session_key_always_lands_on_same_backend() {
        let (config, _dir) = test_config(backends(&[("a", -1), ("b", -1), ("c", -1)]), true);
        let manager = BackendManager::new(config);
        // The pick itself does not pin; upstream pins once a request succeeds.
        let b1 = manager.pick(Some("f:key"), &exclusion(&[])).expect("pick");
        assert_eq!(manager.pin_backend_id("f:key"), None);
        manager.pin_to("f:key", &b1.id);
        let b2 = manager.pick(Some("f:key"), &exclusion(&[])).expect("pick");
        let b3 = manager.pick(Some("f:key"), &exclusion(&[])).expect("pick");
        assert_eq!(b1.id, b2.id);
        assert_eq!(b2.id, b3.id);
        assert_eq!(
            manager.pin_backend_id("f:key").as_deref(),
            Some(b1.id.as_str())
        );
    }

    #[test]
    fn non_sticky_mode_never_pins() {
        let (config, _dir) = test_config(backends(&[("a", -1), ("b", -1)]), false);
        let manager = BackendManager::new(config);
        manager.pick(Some("f:key"), &exclusion(&[])).expect("pick");
        manager.pick(Some("f:key"), &exclusion(&[])).expect("pick");
        assert_eq!(manager.pin_backend_id("f:key"), None);
        assert_eq!(manager.pin_count(), 0);
    }

    #[test]
    fn healthy_filter_excludes_downed_backends() {
        let (config, _dir) = test_config(backends(&[("a", -1), ("b", -1), ("c", -1)]), false);
        let manager = BackendManager::new(config);
        manager.mark_down("b");
        manager.mark_down("c");
        let chosen = manager.pick(None, &exclusion(&[])).expect("pick");
        assert_eq!(chosen.id, "a");
    }

    #[test]
    fn mark_down_flips_backend_to_unhealthy() {
        let (config, _dir) = test_config(backends(&[("a", -1), ("b", -1)]), false);
        let manager = BackendManager::new(config);
        manager.mark_down("a");
        let chosen = manager.pick(None, &exclusion(&[])).expect("pick");
        assert_ne!(chosen.id, "a");
        let report = manager.health_report();
        assert!(!report.iter().find(|b| b.id == "a").expect("a").healthy);
        assert!(report.iter().find(|b| b.id == "b").expect("b").healthy);
    }

    #[test]
    fn priority_lowest_priority_backend_chosen_when_healthy() {
        let (config, _dir) = test_config(backends(&[("a", 0), ("b", 1)]), false);
        let manager = BackendManager::new(config);
        for _ in 0..20 {
            let chosen = manager.pick(None, &exclusion(&[])).expect("pick");
            assert_eq!(chosen.id, "a");
        }
    }

    #[test]
    fn priority_falls_back_to_next_group_when_primary_is_unhealthy() {
        let (config, _dir) = test_config(backends(&[("a", 0), ("b", 1)]), false);
        let manager = BackendManager::new(config);
        manager.mark_down("a");
        for _ in 0..20 {
            let chosen = manager.pick(None, &exclusion(&[])).expect("pick");
            assert_eq!(chosen.id, "b");
        }
    }

    #[test]
    fn backends_without_priority_rank_equal_to_each_other() {
        let (config, _dir) = test_config(backends(&[("a", -1), ("b", -1)]), false);
        let manager = BackendManager::new(config);
        let mut seen = HashSet::new();
        for _ in 0..40 {
            seen.insert(manager.pick(None, &exclusion(&[])).expect("pick").id);
        }
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn unprioritized_backends_are_never_used_while_a_prioritized_one_is_healthy() {
        let (config, _dir) = test_config(backends(&[("a", 1), ("b", -1)]), false);
        let manager = BackendManager::new(config);
        for _ in 0..20 {
            assert_eq!(manager.pick(None, &exclusion(&[])).expect("pick").id, "a");
        }
        manager.mark_down("a");
        for _ in 0..20 {
            assert_eq!(manager.pick(None, &exclusion(&[])).expect("pick").id, "b");
        }
    }

    #[test]
    fn weights_bias_selection_within_a_priority_group() {
        let mut bes = backends(&[("heavy", -1), ("light", -1)]);
        bes[0].weight = 9.0;
        bes[1].weight = 1.0;
        let (config, _dir) = test_config(bes, false);
        let manager = BackendManager::new(config);

        let mut heavy = 0;
        let mut light = 0;
        for _ in 0..400 {
            let chosen = manager.pick(None, &exclusion(&[])).expect("pick");
            if chosen.id == "heavy" {
                heavy += 1;
            } else {
                light += 1;
            }
        }
        assert_eq!(heavy + light, 400);
        // 9:1 weights over 400 draws: expect roughly 360/40, checked loosely.
        assert!(
            heavy > light * 3,
            "expected heavy to dominate: {heavy} vs {light}"
        );
        assert!(
            light > 10,
            "light should still be picked sometimes: {light}"
        );
    }

    #[test]
    fn exclusion_set_skips_backends_for_this_request() {
        let (config, _dir) = test_config(backends(&[("a", 0), ("b", 1)]), false);
        let manager = BackendManager::new(config);
        let chosen = manager.pick(None, &exclusion(&["a"])).expect("pick");
        assert_eq!(chosen.id, "b");
    }

    #[test]
    fn excluded_pinned_session_fails_over_without_losing_the_pin() {
        let (config, _dir) = test_config(backends(&[("a", 0), ("b", 1)]), true);
        let manager = BackendManager::new(config);
        manager.pin_to("f:s", "a");

        // Simulate a rate-limit failover: a is excluded for this request only.
        let failover = manager.pick(Some("f:s"), &exclusion(&["a"])).expect("pick");
        assert_eq!(failover.id, "b");
        assert_eq!(manager.pin_backend_id("f:s").as_deref(), Some("a"));

        // Next turn returns to the pinned backend.
        let next = manager.pick(Some("f:s"), &exclusion(&[])).expect("pick");
        assert_eq!(next.id, "a");
    }

    #[test]
    fn downed_pinned_backend_drops_the_pin() {
        let (config, _dir) = test_config(backends(&[("a", 0), ("b", 1)]), true);
        let manager = BackendManager::new(config);
        manager.pin_to("f:s", "a");

        manager.mark_down("a");
        let failover = manager.pick(Some("f:s"), &exclusion(&[])).expect("pick");
        assert_eq!(failover.id, "b");
        // The stale pin is dropped; the session is only re-pinned once the
        // fallback actually serves a request.
        assert_eq!(manager.pin_backend_id("f:s"), None);
    }

    #[test]
    fn pick_returns_none_when_every_backend_is_unavailable() {
        let (config, _dir) = test_config(backends(&[("a", -1)]), false);
        let manager = BackendManager::new(config);
        manager.mark_down("a");
        assert!(manager.pick(None, &exclusion(&[])).is_none());
    }

    #[test]
    fn pin_to_re_pins_a_session() {
        let (config, _dir) = test_config(backends(&[("a", 0), ("b", 1)]), true);
        let manager = BackendManager::new(config);
        manager.pin_to("f:s", "b");
        assert_eq!(manager.pin_backend_id("f:s").as_deref(), Some("b"));
        assert_eq!(
            manager.pick(Some("f:s"), &exclusion(&[])).expect("pick").id,
            "b"
        );
    }

    #[test]
    fn pin_to_is_ignored_when_not_sticky() {
        let (config, _dir) = test_config(backends(&[("a", 0), ("b", 1)]), false);
        let manager = BackendManager::new(config);
        manager.pin_to("f:s", "b");
        assert_eq!(manager.pin_backend_id("f:s"), None);
    }

    #[test]
    fn pins_persist_across_a_restart() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut config, _dir) = test_config(backends(&[("a", -1), ("b", -1)]), true);
        config.state_dir = Some(dir.path().display().to_string());

        let first = BackendManager::new(config.clone());
        first.pin_to("f:survive", "a");
        first.stop(); // flush to disk

        // Simulate a restart: a brand-new manager reading the same dir.
        let second = BackendManager::new(config);
        let restored = second
            .pick(Some("f:survive"), &exclusion(&[]))
            .expect("pick");
        assert_eq!(restored.id, "a");
    }

    #[test]
    fn expired_pins_are_dropped_on_restore() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut config, _dir) = test_config(backends(&[("a", -1), ("b", -1)]), true);
        config.state_dir = Some(dir.path().display().to_string());

        let first = BackendManager::new(config.clone());
        first.pin_to("f:stale", "a");
        first.stop();

        // Rewrite the persisted pin with an ancient lastSeen.
        let file = crate::pins_store::pins_file(dir.path());
        let mut data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).expect("read")).expect("json");
        data["pins"]["f:stale"]["lastSeen"] = serde_json::json!(now_ms() - 3_600_000);
        std::fs::write(&file, serde_json::to_string(&data).expect("json")).expect("write");

        let second = BackendManager::new(config);
        assert_eq!(
            second.pin_backend_id("f:stale"),
            None,
            "expired pin must be dropped on restore"
        );
    }

    #[test]
    fn pins_for_unknown_backends_are_dropped_on_restore() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut config, _dir) = test_config(backends(&[("a", -1)]), true);
        config.state_dir = Some(dir.path().display().to_string());
        let first = BackendManager::new(config.clone());
        first.pin_to("f:s", "a");
        first.pin_to("f:gone", "removed-backend");
        first.stop();

        let second = BackendManager::new(config);
        assert_eq!(second.pin_backend_id("f:s").as_deref(), Some("a"));
        assert_eq!(second.pin_backend_id("f:gone"), None);
    }

    #[test]
    fn expired_pins_are_evicted_once_the_map_grows_large() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut config, _dir) = test_config(backends(&[("a", -1), ("b", -1)]), true);
        config.state_dir = Some(dir.path().display().to_string());
        config.session_ttl_ms = 60_000;

        let manager = BackendManager::new(config);
        let now = now_ms();
        {
            let mut inner = manager.lock();
            for i in 0..PIN_EVICTION_THRESHOLD + 10 {
                // 90% expired, 10% fresh.
                let last_seen = if i % 10 == 0 { now } else { now - 3_600_000 };
                inner.pins.insert(
                    format!("f:key-{i}"),
                    StoredPin {
                        backend_id: if i % 2 == 0 { "a" } else { "b" }.to_string(),
                        last_seen,
                    },
                );
            }
        }
        // A new pin triggers eviction.
        manager.pin_to("f:fresh", "a");

        let remaining = manager.pin_count();
        assert!(
            remaining < PIN_EVICTION_TARGET,
            "expected eviction down to under {PIN_EVICTION_TARGET}, got {remaining}"
        );
        assert!(manager.pin_backend_id("f:fresh").is_some());
    }

    #[test]
    fn flush_only_writes_when_dirty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut config, _dir) = test_config(backends(&[("a", -1)]), true);
        config.state_dir = Some(dir.path().display().to_string());
        let manager = BackendManager::new(config);

        manager.flush(); // nothing changed yet
        assert!(!crate::pins_store::pins_file(dir.path()).exists());

        manager.pin_to("f:s", "a");
        manager.flush();
        assert!(crate::pins_store::pins_file(dir.path()).exists());
    }

    #[test]
    fn with_path_preserves_the_api_root_and_strips_trailing_slashes() {
        assert_eq!(
            with_path("http://127.0.0.1:3456/v1", "/models"),
            "http://127.0.0.1:3456/v1/models"
        );
        assert_eq!(
            with_path("http://127.0.0.1:3456/v1///", "/models"),
            "http://127.0.0.1:3456/v1/models"
        );
        assert_eq!(with_path("", "/models"), "/models");
    }

    #[tokio::test]
    async fn health_checks_mark_unreachable_backends_down() {
        use crate::testing;
        let server = testing::spawn_test_server(
            axum::Router::new().route("/v1/models", axum::routing::get(|| async { "ok" })),
        )
        .await;

        let mut bes = backends(&[("live", -1), ("dead", -1)]);
        bes[0].base_url = format!("http://127.0.0.1:{}/v1", server.port);
        // Port 1 is not listening: connection refused.
        bes[1].base_url = "http://127.0.0.1:1/v1".to_string();
        let (config, _dir) = test_config(bes, false);
        let manager = BackendManager::new(config);

        manager.check_all(&reqwest::Client::new()).await;

        let report = manager.health_report();
        assert!(
            report
                .iter()
                .find(|b| b.id == "live")
                .expect("live")
                .healthy
        );
        assert!(
            !report
                .iter()
                .find(|b| b.id == "dead")
                .expect("dead")
                .healthy
        );
    }

    #[tokio::test]
    async fn error_status_still_counts_as_healthy() {
        use crate::testing;
        let server = testing::spawn_test_server(axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        ))
        .await;

        let mut bes = backends(&[("live", -1)]);
        bes[0].base_url = format!("http://127.0.0.1:{}/v1", server.port);
        let (config, _dir) = test_config(bes, false);
        let manager = BackendManager::new(config);
        manager.check_all(&reqwest::Client::new()).await;

        assert!(
            manager
                .health_report()
                .iter()
                .find(|b| b.id == "live")
                .expect("live")
                .healthy
        );
    }
}
