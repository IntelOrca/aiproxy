import type { BackendConfig, Config } from "./types.ts";

const DEFAULTS = {
  port: 8080,
  sticky: true,
  healthCheckIntervalMs: 10_000,
  healthCheckTimeoutMs: 2_500,
  sessionTtlMs: 30 * 60_000,
  upstreamTimeoutMs: 600_000,
  maxRetries: 1,
  retryStatusCodes: [429, 529],
  retryOnLimitMessage: true,
  recentRoutes: 200,
};

export function loadConfig(path = "config.json"): Config {
  let partial: any = {};
  try {
    partial = JSON.parse(Deno.readTextFileSync(path));
  } catch {
    console.warn(`[aiproxy] ${path} not found or invalid; using defaults.`);
  }

  const backends: BackendConfig[] = (partial.backends ?? []).map((b: any, i: number) => ({
    id: typeof b.id === "string" && b.id ? b.id : `backend-${i + 1}`,
    baseUrl: String(b.baseUrl ?? "http://127.0.0.1:3456/v1").replace(/\/+$/, ""),
    weight: Number(b.weight ?? 1) || 1,
    model: typeof b.model === "string" ? b.model : undefined,
    apiKey: typeof b.apiKey === "string" && b.apiKey ? b.apiKey : undefined,
    priority: typeof b.priority === "number" ? b.priority : undefined,
  }));

  if (backends.length === 0) {
    console.warn(
      "[aiproxy] no backends configured; defaulting to http://127.0.0.1:3456/v1 (opencode default port).",
    );
    backends.push({
      id: "backend-1",
      baseUrl: "http://127.0.0.1:3456/v1",
      weight: 1,
    });
  }

  return {
    port: Number(partial.port ?? DEFAULTS.port) || DEFAULTS.port,
    backends,
    sticky: partial.sticky ?? DEFAULTS.sticky,
    healthCheckIntervalMs: Number(partial.healthCheckIntervalMs ?? DEFAULTS.healthCheckIntervalMs) ||
      DEFAULTS.healthCheckIntervalMs,
    healthCheckTimeoutMs: Number(partial.healthCheckTimeoutMs ?? DEFAULTS.healthCheckTimeoutMs) ||
      DEFAULTS.healthCheckTimeoutMs,
    sessionTtlMs: Number(partial.sessionTtlMs ?? DEFAULTS.sessionTtlMs) || DEFAULTS.sessionTtlMs,
    upstreamTimeoutMs: Number(partial.upstreamTimeoutMs ?? DEFAULTS.upstreamTimeoutMs) ||
      DEFAULTS.upstreamTimeoutMs,
    maxRetries: Number(partial.maxRetries ?? DEFAULTS.maxRetries) || DEFAULTS.maxRetries,
    models: Array.isArray(partial.models) ? partial.models.map(String) : undefined,
    modelMap: partial.modelMap && typeof partial.modelMap === "object"
      ? partial.modelMap as Record<string, string>
      : undefined,
    apiKeys: Array.isArray(partial.apiKeys)
      ? partial.apiKeys.map(String).filter((k: string) => k.length > 0)
      : undefined,
    retryStatusCodes: Array.isArray(partial.retryStatusCodes)
      ? partial.retryStatusCodes.map(Number).filter((n: number) => Number.isFinite(n))
      : DEFAULTS.retryStatusCodes,
    retryOnLimitMessage: partial.retryOnLimitMessage ?? DEFAULTS.retryOnLimitMessage,
    stateDir: typeof partial.stateDir === "string" && partial.stateDir
      ? partial.stateDir
      : undefined,
    recentRoutes: Number(partial.recentRoutes ?? DEFAULTS.recentRoutes) ||
      DEFAULTS.recentRoutes,
  };
}
