import type { Config } from "./types.ts";
import { BackendManager, withPath } from "./backend.ts";
import { sessionKeyFromBody } from "./session.ts";
import { forwardWithRetry } from "./upstream.ts";

interface RoutingEntry {
  at: string;
  sessionId: string;
  model: string;
  backendId: string;
  endpoint: string;
  status: number;
  ms: number;
}

/** Start the router HTTP server. Blocks; call as the last thing in main. */
export function startAiproxy(config: Config): void {
  const manager = new BackendManager(config);
  manager.startHealthChecks();

  // Flush persisted session pins on shutdown.
  for (const sig of ["SIGINT", "SIGTERM"] as const) {
    try {
      Deno.addSignalListener(sig, () => {
        manager.stop();
        Deno.exit(0);
      });
    } catch {
      // Signal listeners not supported on this platform.
    }
  }

  const backendCounts = new Map<string, number>(config.backends.map((b) => [b.id, 0]));
  const recent: RoutingEntry[] = [];

  async function handleModels(): Promise<Response> {
    if (config.models) {
      return Response.json({
        object: "list",
        data: config.models.map((id) => ({ id, object: "model", owned_by: "aiproxy" })),
      });
    }
    const backend = manager.pick(undefined);
    if (backend) {
      try {
        const upstream = await fetch(withPath(backend.config.baseUrl, "/models"), {
          signal: AbortSignal.timeout(3_000),
        });
        if (upstream.ok) {
          return new Response(upstream.body, {
            status: upstream.status,
            headers: upstream.headers,
          });
        }
      } catch {
        // fall through to default list
      }
    }
    return Response.json({
      object: "list",
      data: [{ id: "gpt-4o", object: "model", owned_by: "aiproxy" }],
    });
  }

  function jsonError(status: number, message: string): Response {
    return Response.json(
      { error: { message, type: "invalid_request_error", code: status } },
      { status },
    );
  }

  /** True when client auth is disabled or the request carries a valid key. */
  function clientAuthorized(req: Request): boolean {
    const keys = config.apiKeys;
    if (!keys || keys.length === 0) return true;
    const m = /^Bearer\s+(.+)$/i.exec(req.headers.get("authorization") ?? "");
    return m !== null && keys.includes(m[1].trim());
  }

  function statusJson(): Response {
    return Response.json({
      service: "aiproxy",
      config: {
        port: config.port,
        sticky: config.sticky !== false,
        retryStatusCodes: config.retryStatusCodes ?? [429, 529],
        retryOnLimitMessage: config.retryOnLimitMessage ?? true,
        authRequired: Array.isArray(config.apiKeys) && config.apiKeys.length > 0,
      },
      models: config.models ?? null,
      backends: manager.healthReport().map((b) => ({
        ...b,
        requests: backendCounts.get(b.id) ?? 0,
      })),
      recent: recent.slice(0, 50),
    });
  }

  function dashboardHtml(): Response {
    return new Response(DASHBOARD_HTML, {
      headers: { "Content-Type": "text/html; charset=utf-8" },
    });
  }

  Deno.serve({ port: config.port, hostname: "127.0.0.1" }, async (req) => {
    const url = new URL(req.url);
    const t0 = performance.now();

    try {
      if (req.method === "OPTIONS") return new Response(null, { status: 204 });

      if (req.method === "GET") {
        if (url.pathname === "/" || url.pathname === "/index.html") return dashboardHtml();
        if (url.pathname === "/api/status") return statusJson();
        if (url.pathname === "/models" || url.pathname === "/v1/models") {
          if (!clientAuthorized(req)) return jsonError(401, "invalid or missing API key");
          return await handleModels();
        }
        return jsonError(404, `no route for GET ${url.pathname}`);
      }

      if (req.method === "POST" && url.pathname.endsWith("/chat/completions")) {
        if (!clientAuthorized(req)) return jsonError(401, "invalid or missing API key");
        const text = await req.text();
        let body: any;
        try {
          body = JSON.parse(text);
        } catch {
          return jsonError(400, "invalid JSON body");
        }

        const sessionKey = await sessionKeyFromBody(req, body);
        const result = await forwardWithRetry(req, body, sessionKey, manager, config);
        const ms = Math.round(performance.now() - t0);

        if (result.backendId !== "none") {
          backendCounts.set(result.backendId, (backendCounts.get(result.backendId) ?? 0) + 1);
          recent.unshift({
            at: new Date().toISOString(),
            sessionId: sessionKey ?? "-",
            model: body?.model ?? "-",
            backendId: result.backendId,
            endpoint: result.endpoint,
            status: result.response.status,
            ms,
          });
          if (recent.length > 200) recent.pop();
        }

        console.log(
          `[aiproxy] ${req.method} ${url.pathname} session=${sessionKey ?? "-"} ` +
            `-> ${result.backendId}${result.endpoint ? ` (${result.endpoint})` : ""} ` +
            `model=${body?.model ?? "-"} status=${result.response.status} ${ms}ms`,
        );
        return result.response;
      }

      return jsonError(404, `no route for ${req.method} ${url.pathname}`);
    } catch (err) {
      console.error("[aiproxy] error:", err);
      return jsonError(500, err instanceof Error ? err.message : String(err));
    }
  });
}

const DASHBOARD_HTML = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>aiproxy dashboard</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { font-family: ui-monospace, SFMono-Regular, Consolas, monospace; background: #0f1117; color: #d7dae0; margin: 0; padding: 24px; }
  h1 { font-size: 20px; margin: 0 0 4px; color: #fff; }
  .sub { color: #8b93a3; font-size: 12px; margin-bottom: 8px; }
  #meta { font-size: 12px; color: #8b93a3; margin-bottom: 4px; }
  h2 { font-size: 14px; text-transform: uppercase; letter-spacing: .08em; color: #8b93a3; margin: 24px 0 8px; }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th, td { text-align: left; padding: 6px 10px; border-bottom: 1px solid #1e2430; white-space: nowrap; }
  th { color: #8b93a3; font-weight: 500; }
  td.sid { font-size: 12px; color: #a5b4fc; }
  .dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 6px; }
  .up { background: #34d399; }
  .down { background: #f87171; }
  .badge { display: inline-block; padding: 2px 8px; border-radius: 10px; font-size: 11px; background: #1e2430; color: #a5b4fc; margin: 2px 4px 2px 0; }
  .ok { color: #34d399; }
  .err { color: #f87171; }
</style>
</head>
<body>
<h1>aiproxy</h1>
<div class="sub">OpenAI-compatible completions router</div>
<div id="meta"></div>
<h2>Backends</h2>
<table>
  <thead><tr><th></th><th>id</th><th>endpoint</th><th>prio</th><th>requests</th></tr></thead>
  <tbody id="backends"></tbody>
</table>
<h2>Models</h2>
<div id="models"></div>
<h2>Recent routes</h2>
<table>
  <thead><tr><th>time</th><th>internal session id</th><th>model</th><th>backend</th><th>endpoint</th><th>status</th><th>ms</th></tr></thead>
  <tbody id="routes"></tbody>
</table>
<script>
function el(tag, cls, text) {
  var e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}
function render(d) {
  document.getElementById('meta').textContent =
    'port ' + d.config.port + ' | sticky ' + d.config.sticky +
    (d.config.authRequired ? ' | auth required' : ' | auth off');
  var tb = document.getElementById('backends');
  tb.textContent = '';
  d.backends.forEach(function (b) {
    var tr = el('tr');
    var td = el('td');
    td.appendChild(el('span', 'dot ' + (b.healthy ? 'up' : 'down')));
    tr.appendChild(td);
    tr.appendChild(el('td', null, b.id));
    tr.appendChild(el('td', null, b.baseUrl));
    tr.appendChild(el('td', null, b.priority === undefined ? '-' : String(b.priority)));
    tr.appendChild(el('td', null, String(b.requests)));
    tb.appendChild(tr);
  });
  var tm = document.getElementById('models');
  tm.textContent = '';
  (d.models || []).forEach(function (m) {
    tm.appendChild(el('span', 'badge', m));
  });
  var tr2 = document.getElementById('routes');
  tr2.textContent = '';
  d.recent.forEach(function (r) {
    var row = el('tr');
    row.appendChild(el('td', null, new Date(r.at).toLocaleTimeString()));
    row.appendChild(el('td', 'sid', r.sessionId));
    row.appendChild(el('td', null, r.model));
    row.appendChild(el('td', null, r.backendId));
    row.appendChild(el('td', null, r.endpoint));
    var st = el('td', Number(r.status) < 400 ? 'ok' : 'err', String(r.status));
    row.appendChild(st);
    row.appendChild(el('td', null, String(r.ms)));
    tr2.appendChild(row);
  });
}
async function refresh() {
  try {
    var d = await (await fetch('/api/status')).json();
    render(d);
  } catch (e) { /* ignore transient errors */ }
}
refresh();
setInterval(refresh, 2000);
</script>
</body>
</html>`;
