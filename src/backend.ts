import type { Config } from "./types.ts";
import { defaultStateDir, loadPins, savePins, type StoredPin } from "./pins-store.ts";

interface BackendState {
  config: Config["backends"][number];
  healthy: boolean;
}

/** Append a path to an OpenAI-compatible API root, preserving any base prefix. */
export function withPath(baseUrl: string, path: string): string {
  return baseUrl.replace(/\/+$/, "") + path;
}

export class BackendManager {
  private states: BackendState[];
  private pins = new Map<string, StoredPin>();
  private timer: ReturnType<typeof setInterval> | undefined;
  private saveTimer: ReturnType<typeof setTimeout> | undefined;
  private dirty = false;
  private pinsDir: string;

  constructor(private config: Config) {
    this.states = config.backends.map((b) => ({ config: b, healthy: true }));
    this.pinsDir = config.stateDir ?? defaultStateDir();
    this.restorePins();
  }

  /** Load persisted pins for backends that still exist, dropping expired ones. */
  private restorePins(): void {
    const validIds = new Set(this.config.backends.map((b) => b.id));
    const now = Date.now();
    let restored = 0;
    for (const [key, pin] of loadPins(this.pinsDir)) {
      if (!validIds.has(pin.backendId)) continue;
      if (now - pin.lastSeen > this.config.sessionTtlMs) continue;
      this.pins.set(key, pin);
      restored++;
    }
    if (restored > 0) {
      console.log(`[aiproxy] restored ${restored} session pin(s) from ${this.pinsDir}`);
    }
  }

  /** Schedule a persisted write of the current pins (debounced). */
  private persistSoon(): void {
    this.dirty = true;
    if (this.saveTimer) return;
    this.saveTimer = setTimeout(() => {
      this.saveTimer = undefined;
      this.flushPins();
    }, 500);
  }

  /** Write pins to disk if anything changed. */
  private flushPins(): void {
    if (!this.dirty) return;
    this.dirty = false;
    savePins(this.pinsDir, this.pins);
  }

  startHealthChecks(): void {
    this.checkAll();
    this.timer = setInterval(() => {
      this.checkAll();
      // Opportunistically flush any pending pin changes.
      this.flushPins();
    }, this.config.healthCheckIntervalMs);
  }

  stop(): void {
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = undefined;
    }
    if (this.saveTimer) {
      clearTimeout(this.saveTimer);
      this.saveTimer = undefined;
    }
    this.flushPins();
  }

  private async checkOne(state: BackendState): Promise<void> {
    try {
      const res = await fetch(withPath(state.config.baseUrl, "/models"), {
        method: "GET",
        signal: AbortSignal.timeout(this.config.healthCheckTimeoutMs),
      });
      // Any HTTP response (even 4xx) means the backend is reachable.
      void res;
      state.healthy = true;
    } catch {
      state.healthy = false;
    }
  }

  async checkAll(): Promise<void> {
    await Promise.all(this.states.map((s) => this.checkOne(s)));
  }

  healthReport(): {
    id: string;
    baseUrl: string;
    healthy: boolean;
    priority?: number;
  }[] {
    return this.states.map((s) => ({
      id: s.config.id,
      baseUrl: s.config.baseUrl,
      healthy: s.healthy,
      priority: s.config.priority,
    }));
  }

  /**
   * Pick a backend. With `sticky` enabled and a sessionKey, returns the backend
   * this conversation was pinned to (if still healthy and not excluded). If the
   * pinned backend is healthy but excluded (rate-limit failover in progress),
   * picks another backend WITHOUT re-pinning, so the conversation keeps its
   * preferred backend for next time. If the pinned backend is unhealthy, the
   * pin is dropped and a fresh backend is picked (and pinned).
   *
   * New picks prefer the lowest `priority` group of healthy backends and do a
   * weighted random pick within it.
   */
  pick(
    sessionKey: string | undefined,
    exclude?: Set<string>,
  ): BackendState | null {
    const pool = this.states.filter((s) =>
      s.healthy && !exclude?.has(s.config.id)
    );
    if (pool.length === 0) return null;

    const pinnedId = this.config.sticky !== false && sessionKey
      ? this.pins.get(sessionKey)?.backendId
      : undefined;

    if (pinnedId && sessionKey) {
      const pinned = pool.find((s) => s.config.id === pinnedId);
      if (pinned) {
        // Preferred backend is healthy and not excluded — stick with it.
        const pin = this.pins.get(sessionKey)!;
        pin.lastSeen = Date.now();
        this.persistSoon();
        return pinned;
      }
      const pinnedHealthy = this.states.find((s) => s.config.id === pinnedId)
        ?.healthy;
      if (pinnedHealthy) {
        // Excluded for this request (e.g. rate-limited) — use another backend
        // without touching the pin. upstream re-pins the session to whichever
        // backend actually serves the request.
        return this.weightedPick(pool);
      }
      // Pinned backend is down — drop the pin and pick fresh below.
      this.pins.delete(sessionKey);
      this.persistSoon();
    }

    const chosen = this.weightedPick(pool);
    if (this.config.sticky !== false && sessionKey) {
      this.pins.set(sessionKey, {
        backendId: chosen.config.id,
        lastSeen: Date.now(),
      });
      this.evictPins();
      this.persistSoon();
    }
    return chosen;
  }

  private weightedPick(pool: BackendState[]): BackendState {
    // Prefer the lowest-priority group (backends without a priority are equal
    // and rank below any explicitly prioritized backend).
    const eff = (s: BackendState) =>
      s.config.priority ?? Number.MAX_SAFE_INTEGER;
    const minPriority = Math.min(...pool.map(eff));
    const candidates = pool.filter((s) => eff(s) === minPriority);

    const total = candidates.reduce((acc, s) => acc + s.config.weight, 0);
    let r = Math.random() * total;
    let chosen = candidates[0];
    for (const s of candidates) {
      r -= s.config.weight;
      if (r <= 0) {
        chosen = s;
        break;
      }
    }
    return chosen;
  }

  /** Re-pin a session to a specific backend (e.g. after a retry/failover). */
  pinTo(sessionKey: string, backendId: string): void {
    if (this.config.sticky === false) return;
    this.pins.set(sessionKey, { backendId, lastSeen: Date.now() });
    this.persistSoon();
  }

  /** Mark a backend unhealthy after a failed forward. */
  markDown(backendId: string): void {
    const st = this.states.find((s) => s.config.id === backendId);
    if (st) st.healthy = false;
  }

  private evictPins(): void {
    if (this.pins.size < 10_000) return;
    const ttl = this.config.sessionTtlMs;
    const now = Date.now();
    for (const [k, v] of this.pins) {
      if (now - v.lastSeen > ttl) this.pins.delete(k);
      if (this.pins.size < 8_000) break;
    }
  }
}
