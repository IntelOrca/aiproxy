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
  shutdownGraceMs: 10_000,
};

export function loadConfig(path = "config.json"): Config {
  let partial: Record<string, unknown> = {};
  try {
    const parsed: unknown = JSON.parse(Deno.readTextFileSync(path));
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      partial = parsed as Record<string, unknown>;
    }
  } catch {
    console.warn(`[aiproxy] ${path} not found or invalid; using defaults.`);
  }

  const rawBackends = Array.isArray(partial.backends)
    ? partial.backends as unknown[]
    : [];
  const backends: BackendConfig[] = rawBackends.map((b: unknown, i: number) => {
    const be = b && typeof b === "object" && !Array.isArray(b)
      ? b as Record<string, unknown>
      : {};
    return {
      id: typeof be.id === "string" && be.id ? be.id : `backend-${i + 1}`,
      baseUrl: String(be.baseUrl ?? "http://127.0.0.1:3456/v1").replace(
        /\/+$/,
        "",
      ),
      weight: Number(be.weight ?? 1) || 1,
      model: typeof be.model === "string" ? be.model : undefined,
      apiKey: typeof be.apiKey === "string" && be.apiKey
        ? be.apiKey
        : undefined,
      priority: typeof be.priority === "number" ? be.priority : undefined,
    };
  });

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
    sticky: Boolean(partial.sticky ?? DEFAULTS.sticky),
    healthCheckIntervalMs:
      Number(partial.healthCheckIntervalMs ?? DEFAULTS.healthCheckIntervalMs) ||
      DEFAULTS.healthCheckIntervalMs,
    healthCheckTimeoutMs:
      Number(partial.healthCheckTimeoutMs ?? DEFAULTS.healthCheckTimeoutMs) ||
      DEFAULTS.healthCheckTimeoutMs,
    sessionTtlMs: Number(partial.sessionTtlMs ?? DEFAULTS.sessionTtlMs) ||
      DEFAULTS.sessionTtlMs,
    upstreamTimeoutMs:
      Number(partial.upstreamTimeoutMs ?? DEFAULTS.upstreamTimeoutMs) ||
      DEFAULTS.upstreamTimeoutMs,
    maxRetries: Number(partial.maxRetries ?? DEFAULTS.maxRetries) ||
      DEFAULTS.maxRetries,
    models: Array.isArray(partial.models)
      ? partial.models.map(String)
      : undefined,
    modelMap: partial.modelMap && typeof partial.modelMap === "object"
      ? partial.modelMap as Record<string, string>
      : undefined,
    apiKeys: Array.isArray(partial.apiKeys)
      ? partial.apiKeys.map(String).filter((k: string) => k.length > 0)
      : undefined,
    retryStatusCodes: Array.isArray(partial.retryStatusCodes)
      ? partial.retryStatusCodes.map(Number).filter((n: number) =>
        Number.isFinite(n)
      )
      : DEFAULTS.retryStatusCodes,
    retryOnLimitMessage: Boolean(
      partial.retryOnLimitMessage ?? DEFAULTS.retryOnLimitMessage,
    ),
    stateDir: typeof partial.stateDir === "string" && partial.stateDir
      ? partial.stateDir
      : undefined,
    recentRoutes: Number(partial.recentRoutes ?? DEFAULTS.recentRoutes) ||
      DEFAULTS.recentRoutes,
    shutdownGraceMs: Number(partial.shutdownGraceMs ?? DEFAULTS.shutdownGraceMs) ||
      DEFAULTS.shutdownGraceMs,
  };
}
