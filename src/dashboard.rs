//! The live dashboard served from `GET /`.
//!
//! Ported verbatim from the Deno version so the UI is unchanged; it polls
//! `/api/status` every two seconds.

pub const DASHBOARD_HTML: &str = r####"<!DOCTYPE html>
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
</html>"####;
