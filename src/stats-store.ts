import { readJson, writeJsonAtomic } from "./pins-store.ts";
import type { RoutingEntry } from "./types.ts";

/**
 * Persistence for backend request stats and the recent-routes log so a
 * restart restores the dashboard to its previous state.
 */

const STATS_FILE_VERSION = 1;

export interface StatsSnapshot {
  backendCounts: Record<string, number>;
  recent: RoutingEntry[];
}

function isValidEntry(e: unknown): e is RoutingEntry {
  if (!e || typeof e !== "object") return false;
  const r = e as Partial<RoutingEntry>;
  return typeof r.at === "string" &&
    typeof r.sessionId === "string" &&
    typeof r.model === "string" &&
    typeof r.backendId === "string" &&
    typeof r.endpoint === "string" &&
    typeof r.status === "number" &&
    typeof r.ms === "number";
}

/** Load persisted stats. Returns empty stats when nothing is stored yet. */
export function loadStats(stateDir: string): StatsSnapshot {
  const data = readJson(stateDir, "stats.json") as {
    version?: number;
    backendCounts?: Record<string, number>;
    recent?: unknown[];
  } | null;
  if (data?.version !== STATS_FILE_VERSION) return { backendCounts: {}, recent: [] };

  const backendCounts: Record<string, number> = {};
  if (data.backendCounts && typeof data.backendCounts === "object") {
    for (const [id, n] of Object.entries(data.backendCounts)) {
      if (typeof n === "number" && Number.isFinite(n) && n >= 0) backendCounts[id] = n;
    }
  }

  const recent = Array.isArray(data.recent)
    ? data.recent.filter(isValidEntry)
    : [];

  return { backendCounts, recent };
}

/** Persist stats atomically. Best-effort. */
export function saveStats(
  stateDir: string,
  backendCounts: Record<string, number>,
  recent: RoutingEntry[],
): void {
  writeJsonAtomic(stateDir, "stats.json", {
    version: STATS_FILE_VERSION,
    backendCounts,
    recent,
  });
}
