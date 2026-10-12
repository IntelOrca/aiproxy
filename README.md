# aiproxy

[![License: MIT](https://img.shields.io/github/license/IntelOrca/aiproxy)](LICENSE)
[![CI](https://github.com/IntelOrca/aiproxy/actions/workflows/ci.yml/badge.svg)](https://github.com/IntelOrca/aiproxy/actions/workflows/ci.yml)

aiproxy is a local, OpenAI-compatible completions router and load balancer
written in Rust. AI clients such as GitHub Copilot CLI point at its base URL,
and it forwards each request to one of several backends (for example multiple
opencode instances).

Each conversation is pinned to one backend for its lifetime. Provider prompt
caching is content-addressed per account, so keeping a conversation on a single
backend keeps that backend's prompt cache warm: later turns only bill for new
tokens instead of the full prefix every time.

<img src="docs\dashboard.jpg" alt="Dashboard" height="300">

## Key features

- Weighted random selection among healthy backends for new conversations.
- Priority backends are always tried first while healthy.
- Sticky sessions keep each conversation on the same backend.
- Periodic health checks, with failed forwards marking a backend down.
- Retry and failover on transport errors and on rate-limit responses (HTTP
  429/529 or rate-limit wording in the error body). The conversation is
  re-pinned to whichever backend actually served it.
- Optional model name rewriting per backend or via a global map.
- Optional client API key authentication.
- SSE streaming passthrough for chat completions.
- Live HTML dashboard showing backend health and recent routes.
- Graceful shutdown on Ctrl+C/SIGTERM, draining in-flight requests.
- State persistence so session pins and dashboard stats survive restarts.

## Running

You need a recent stable Rust toolchain (developed and tested against 1.98), or
Docker (see below).

### From source

Copy the example config and edit it to match your backends:

```bash
cp config.example.json config.json
```

Then start the router:

- Windows: `aiproxy.bat`
- Linux/macOS: `./aiproxy.sh`

The router accepts `--config <path>` to select a config file (default
`config.json`), `--port <port>` to override the configured port, and
`--bind <address>` to choose where it listens. The launchers build a release
binary on first run and reuse `target/release/aiproxy` afterwards; you can also
run `cargo run --release --` directly.

`--bind` takes a host, a host and port, or `*:PORT` for every interface:

```bash
aiproxy -b localhost            # listen on localhost, with the configured port
aiproxy -b '*:9090'             # listen on every interface, port 9090
aiproxy -b 192.168.1.20:9090    # listen on one interface and port
```

A port given in the bind address overrides the configured `port`; `--port` is a
plain override on its own. Passing both with different ports is a contradiction,
so the router refuses to start:

```bash
aiproxy -b '*:9090' --port 8080   # error: two different ports
aiproxy -b '*:9090' --port 9090   # fine: they agree
```

### With Docker

The multi-stage Dockerfile compiles a statically linked Rust binary (rustls
means no OpenSSL dependency) and runs it on a minimal scratch image as a
non-root user, exposing port 8080.

```bash
docker build -t aiproxy .
docker run --rm -p 8080:8080 \
  -v "$PWD/config.json:/config.json" \
  aiproxy
```

Without a mounted config the image falls back to `config.example.json`. Backends
must be reachable from inside the container.

### Sandbox demo

No real backends needed: the sandbox starts a router plus three fake backends so
you can watch load balancing, sticky sessions, and the dashboard.

```bash
./aiproxy.sh --sandbox -p 6060      # or: aiproxy.bat --sandbox -p 6060
```

The router listens on 6060 and the fake backends on 6061, 6062, and 6063. Open
http://127.0.0.1:6060/ in a browser for the live dashboard. `-b`/`--bind` works
here too: `./aiproxy.sh --sandbox -b '*:6060'` moves the whole cluster.

## Configuration

All settings live in `config.json`. `config.example.json` is the template and
`config.sandbox.json` configures the sandbox cluster.

| Key                     | Default      | Description                                                                  |
| ----------------------- | ------------ | ---------------------------------------------------------------------------- |
| `host`                  | `127.0.0.1`  | Router listen host; `*` or `0.0.0.0` means every interface.                   |
| `port`                  | `8080`       | Router listen port.                                                          |
| `backends`              | required     | Array of backends, each `{id, baseUrl, weight, model?, apiKey?, priority?}`. |
| `sticky`                | `true`       | Pin each conversation to one backend.                                        |
| `healthCheckIntervalMs` | `10000`      | How often backends are health-checked.                                       |
| `healthCheckTimeoutMs`  | `2500`       | Health check timeout.                                                        |
| `sessionTtlMs`          | `1800000`    | How long a session pin is remembered (30 minutes).                           |
| `upstreamTimeoutMs`     | `600000`     | Per-request upstream timeout.                                                |
| `maxRetries`            | `1`          | Failover attempts to other backends per request.                             |
| `retryStatusCodes`      | `[429, 529]` | Upstream status codes treated as rate limited.                               |
| `retryOnLimitMessage`   | `true`       | Also fail over on rate-limit wording in the error body.                      |
| `models`                | proxied      | Static model list advertised by `GET /v1/models`.                            |
| `modelMap`              | `{}`         | Client model to backend model rewrites.                                      |
| `apiKeys`               | `[]`         | Client API keys required on incoming requests.                               |
| `stateDir`              | platform dir | Directory for persisted state (pins, stats).                                 |
| `recentRoutes`          | `200`        | Recent route entries to keep and persist.                                    |
| `shutdownGraceMs`       | `10000`      | Max time to wait for in-flight requests to drain on shutdown.                |

Backend fields:

- `id`: unique identifier, shown in logs and the dashboard.
- `baseUrl`: OpenAI-compatible API root of the backend, for example
  `http://127.0.0.1:3456/v1`.
- `weight`: relative share of new conversations (default 1).
- `priority`: lower numbers are tried first while healthy. Backends without a
  priority rank equally and are only used when no prioritized backend is
  healthy.
- `model`: optional model name substituted when forwarding to this backend.
- `apiKey`: Authorization header sent to this backend.

If `models` is omitted, the router proxies the model list from a healthy backend
instead. `modelMap` rewrites the model the client asked for into what the
backend expects, applied on forward after the per-backend `model` override:

```json
"modelMap": { "gpt-4o": "claude-3-5-sonnet" }
```

When `apiKeys` is set, clients must authenticate to the router with one of the
listed keys or receive a 401. The client's key is not forwarded upstream; use
the per-backend `apiKey` if a backend needs its own.

Unknown or invalid values in the config fall back to the defaults above, so a
partial or malformed config still starts the router. As in previous versions, a
numeric setting of `0` (or a non-numeric value) also falls back to its default.

### Persisted state

Session to backend pins, backend request counts, and the recent routes log are
persisted to disk so a restart does not scatter conversations or reset the
dashboard. State lives in `%LOCALAPPDATA%\aiproxy\` on Windows and
`$XDG_CACHE_HOME/aiproxy/` or `~/.cache/aiproxy/` on Linux and macOS; override
it with `stateDir` in the config. On startup, pins are restored when their
backend still exists in the config and the pin is newer than `sessionTtlMs`;
expired or orphaned pins are dropped, as are recent routes from backends no
longer configured. Writes are debounced and atomic (temp file plus rename).

`recentRoutes` (default 200) caps both the dashboard's recent routes table and
the persisted stats.

## Endpoints

| Route                       | Behavior                                                       |
| --------------------------- | -------------------------------------------------------------- |
| `POST /v1/chat/completions` | Forwarded to a healthy backend with SSE streaming passthrough. |
| `GET /v1/models`            | Static list from config, or proxied from a healthy backend.    |
| `GET /`                     | Live dashboard with backend health and recent routes.          |

## Tests

```bash
cargo test          # unit and integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

All three run in CI on every push and pull request; the workflow also builds the
Docker image. Unit tests live beside the modules they cover (mirroring the old
`*_test.ts` colocated tests) and `tests/e2e.rs` boots the real router against
fake backends.

## License

MIT, copyright Ted John (2026). See [LICENSE](LICENSE).
