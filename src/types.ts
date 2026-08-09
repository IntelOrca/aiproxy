export interface BackendConfig {
  id: string;
  /** OpenAI-compatible API root, e.g. http://127.0.0.1:3456/v1 */
  baseUrl: string;
  weight: number;
  /** Optional model name to substitute when forwarding to this backend */
  model?: string;
  /** API key to send to this backend (overrides the client's Authorization header) */
  apiKey?: string;
  /**
   * Routing preference: lower = tried first. Backends with no priority are
   * equal and only used when every lower-priority backend is unhealthy or
   * rate-limited. Set 0 on your primary backend, 1 on the fallbacks.
   */
  priority?: number;
}

export interface Config {
  port: number;
  backends: BackendConfig[];
  /** Route each conversation to the same backend (recommended for cache affinity) */
  sticky: boolean;
  healthCheckIntervalMs: number;
  healthCheckTimeoutMs: number;
  sessionTtlMs: number;
  upstreamTimeoutMs: number;
  maxRetries: number;
  /** Static model list served by GET /v1/models. If unset, proxied from a healthy backend. */
  models?: string[];
  /** Map client model name -> backend model name, applied on forward */
  modelMap?: Record<string, string>;
  /**
   * Optional list of client API keys. If set, clients must send
   * `Authorization: Bearer <one of these>`; otherwise the router replies 401.
   */
  apiKeys?: string[];
  /** Upstream HTTP status codes that count as "limit reached" and trigger failover (default [429, 529]) */
  retryStatusCodes?: number[];
  /** Also fail over when an upstream error body mentions rate limiting / quota (default true) */
  retryOnLimitMessage?: boolean;
  /**
   * Directory for persisted state (session pins). Defaults to the platform
   * state dir: %LOCALAPPDATA%\aiproxy (Windows) or ~/.cache/aiproxy (elsewhere).
   */
  stateDir?: string;
  /** How many recent route entries to keep and persist (default 200) */
  recentRoutes?: number;
}

/** A single routed request, shown in the dashboard and persisted for restarts. */
export interface RoutingEntry {
  at: string;
  sessionId: string;
  model: string;
  backendId: string;
  endpoint: string;
  status: number;
  ms: number;
}
