import type { Config } from "./types.ts";
import { BackendManager, withPath } from "./backend.ts";

const HOP_BY_HOP = [
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
  "host",
  "content-length",
  "content-encoding",
];

export interface ForwardResult {
  response: Response;
  backendId: string;
  /** baseUrl of the backend that handled the request ("" if none) */
  endpoint: string;
}

const DEFAULT_RETRY_CODES = [429, 529];
const RATE_LIMIT_RE =
  /rate\s*limit|rate_limit|insufficient_quota|limit\s*reached|reached.{0,12}limit|too many requests|overloaded|quota/i;

export async function forwardWithRetry(
  req: Request,
  body: any,
  sessionKey: string | undefined,
  manager: BackendManager,
  config: Config,
): Promise<ForwardResult> {
  const maxAttempts = Math.max(1, config.maxRetries + 1);
  const tried = new Set<string>();
  const retryCodes = new Set(config.retryStatusCodes ?? DEFAULT_RETRY_CODES);
  const scanBody = config.retryOnLimitMessage !== false;

  for (let attempt = 0; attempt < maxAttempts; attempt++) {
    const state = manager.pick(sessionKey, tried);
    if (!state) break;
    tried.add(state.config.id);
    let response: Response;
    try {
      response = await forwardOnce(req, body, state.config, config);
    } catch (err) {
      console.warn(
        `[aiproxy] backend ${state.config.id} failed: ${err instanceof Error ? err.message : err}`,
      );
      manager.markDown(state.config.id);
      if (attempt < maxAttempts - 1 && sessionKey) {
        // Re-pin the session to whichever backend we fail over to next.
        const next = manager.pick(sessionKey, tried);
        if (next) manager.pinTo(sessionKey, next.config.id);
      }
      continue;
    }

    if (
      attempt < maxAttempts - 1 &&
      await isRateLimited(response, retryCodes, scanBody)
    ) {
      // "Limit reached": fail over to another backend for THIS request. Do not
      // markDown — a rate limit isn't "down". The session is re-pinned to the
      // backend that actually serves it (see below).
      console.warn(
        `[aiproxy] backend ${state.config.id} limit reached (status ${response.status}); failing over`,
      );
      continue;
    }

    // This request was served after a retry — pin the conversation to the
    // backend that actually handled it, so the warm prompt cache there is
    // reused next turn instead of bouncing back to the rate-limited one.
    if (attempt > 0 && sessionKey) manager.pinTo(sessionKey, state.config.id);
    return { response, backendId: state.config.id, endpoint: state.config.baseUrl };
  }

  return {
    backendId: "none",
    endpoint: "",
    response: Response.json(
      {
        error: {
          message: "no healthy backends available",
          type: "server_error",
          code: 502,
        },
      },
      { status: 502 },
    ),
  };
}

/** True when the upstream response indicates rate limiting / quota exhaustion. */
async function isRateLimited(
  response: Response,
  codes: Set<number>,
  scanBody: boolean,
): Promise<boolean> {
  if (codes.has(response.status)) return true;
  if (!scanBody || response.status < 400) return false;
  try {
    const text = await response.clone().text();
    return RATE_LIMIT_RE.test(text);
  } catch {
    return false;
  }
}

async function forwardOnce(
  req: Request,
  body: any,
  backend: Config["backends"][number],
  config: Config,
): Promise<Response> {
  const url = new URL(req.url);
  const pathname = url.pathname;
  // Normalize: map any client route suffix onto the backend's API root.
  let targetPath: string;
  if (pathname.endsWith("/models")) {
    targetPath = "/models";
  } else if (pathname.endsWith("/chat/completions")) {
    targetPath = "/chat/completions";
  } else {
    targetPath = pathname;
  }
  const target = withPath(backend.baseUrl, targetPath) + (url.search || "");

  const headers = new Headers(req.headers);
  for (const h of HOP_BY_HOP) headers.delete(h);
  headers.set("host", new URL(backend.baseUrl).host);

  // When the router has its own client keys, the client's key is the router's
  // credential — don't leak it upstream. A per-backend apiKey (if any) then
  // provides the upstream Authorization header instead.
  if (Array.isArray(config.apiKeys) && config.apiKeys.length > 0) {
    headers.delete("authorization");
  }
  if (backend.apiKey) {
    headers.set("authorization", `Bearer ${backend.apiKey}`);
  }

  // Optional model rewrite: per-backend override wins, then global map.
  if (body && typeof body === "object" && typeof body.model === "string") {
    const mapped = backend.model ?? config.modelMap?.[body.model];
    if (mapped) body = { ...body, model: mapped };
  }

  const upstream = await fetch(target, {
    method: req.method,
    headers,
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(config.upstreamTimeoutMs),
  });

  const resHeaders = new Headers();
  for (const [k, v] of upstream.headers) {
    if (HOP_BY_HOP.includes(k.toLowerCase())) continue;
    resHeaders.set(k, v);
  }

  return new Response(upstream.body, {
    status: upstream.status,
    statusText: upstream.statusText,
    headers: resHeaders,
  });
}
