# aiproxy

A local **OpenAI-compatible completions router / load balancer** written in Deno + TypeScript.
Point your AI client (e.g. GitHub Copilot CLI) at it; it forwards requests to one of several
backends (e.g. three separate opencode instances) and keeps each conversation pinned to one
backend so provider prompt-caching actually pays off.

## Running it

`aiproxy.bat` is the single entry point:

| Command                                    | What it does                                            |
| ------------------------------------------ | ------------------------------------------------------- |
| `aiproxy.bat --sandbox -p 6060`            | Sandbox cluster: router on 6060 + 3 fake backends on 6061-6063 |
| `aiproxy.bat`                              | Router using `config.json`                              |
| `aiproxy.bat --config config.alt.json`     | Router using another config file                        |
| `aiproxy.bat -p 9090`                      | Router with the port from config overridden to 9090     |

Router mode needs `--allow-net --allow-read`; sandbox mode needs `--allow-write` (request log)
too — both are wired up in the `.bat`.

## Why sticky routing (your caching question)

Provider prompt caching (OpenAI/Anthropic "cached input tokens") is **content-addressed and
stored per API key/account**:

- **Correctness** — no. Any account can serve any prompt; the cache works regardless of which
  backend the request lands on.
- **Savings** — yes, mostly. Because the cache is per-account, if a multi-turn conversation
  bounces between 3 backends, each account's cache stays *cold* for that conversation's prefix
  and you pay full prefix cost every turn. Sticky routing (same session → same backend) warms
  the cache after turn 1, so later turns only bill for new tokens.

**Reality check from a live capture:** GitHub Copilot CLI (BYOK, OpenAI provider) sends *no
session identifier at all* — no session header, no `session_id` in the body. It resends the
full `messages` history on every request. So the router fingerprints the conversation using the
**first user message** (stable across all turns, distinct between sessions) plus the model.

Session key precedence:

1. `X-Session-ID` request header — explicit, wins over everything.
2. `session_id` / `thread_id` field in the request body (some clients send one).
3. `sha256(model + first user message content)` — the Copilot CLI case.

## Setup

Run N opencode instances on separate ports (one per "account"/config):

```bash
OPENCODE_PORT=3456 opencode serve --port 3456 --hostname 127.0.0.1
OPENCODE_PORT=3457 opencode serve --port 3457 --hostname 127.0.0.1
OPENCODE_PORT=3458 opencode serve --port 3458 --hostname 127.0.0.1
```

> Each opencode instance has its own credentials/config (`.opencode/` per working dir or
> `OPENCODE_CONFIG` pointing at a different config), so they act as separate accounts.

Configure the router:

```bash
cp config.example.json config.json
# edit baseUrls/weights to match your instances
deno task start
```

## Point Copilot CLI at it

```bash
export COPILOT_PROVIDER_BASE_URL=http://127.0.0.1:8080/v1
export COPILOT_PROVIDER_TYPE=openai
export COPILOT_PROVIDER_API_KEY=anything   # the CLI masks it anyway ("******")
export COPILOT_MODEL=gpt-4o                # must match a model your backends accept
copilot
```

## Endpoints

| Route                              | Behavior                                                        |
| ---------------------------------- | --------------------------------------------------------------- |
| `POST /v1/chat/completions`        | Forward to a healthy backend, stream (SSE) passthrough.         |
| `GET /v1/models`                   | Static list from config, or proxied live from a healthy backend.|
| `GET /`                            | Live HTML dashboard (health, request counts, recent routes). |

## Behavior

- **Weighted random** selection among healthy backends for new conversations (weight via
  `weight` in config).
- **Priority (primary first)**: give a backend a lower `priority` number and it is always tried
  first while healthy. Only when it's down or rate-limited do other backends get used. Backends
  without a `priority` rank equally and only run when no prioritized backend is healthy.
- **Sticky sessions**: once a conversation is routed, it stays on that backend until it goes
  unhealthy or the pin expires (`sessionTtlMs`, 30 min default).
- **Health checks**: `GET {backend}/models` every `healthCheckIntervalMs`; any HTTP response
  counts as reachable. Failed forwards also mark a backend down.
- **Retry/failover**: on transport errors, retry on another healthy backend (up to
  `maxRetries`), re-pinning the session. A "limit reached" response (HTTP 429/529, or an error
  body mentioning rate limit / quota, configurable via `retryStatusCodes` /
  `retryOnLimitMessage`) also fails over to another backend for that request, and the session
  is **re-pinned to whichever backend actually served it** — so the warm prompt cache there is
  reused next turn instead of bouncing back to the rate-limited one. A rate limit never marks a
  backend down.
- **Model mapping**: optionally rewrite the client's model name per backend (`backend.model`)
  or globally (`modelMap`).
- **Client auth**: if `apiKeys` is configured, every request to `/v1/chat/completions` and
  `/v1/models` must carry `Authorization: Bearer <one of the keys>` (else 401). Per-backend
  `apiKey` sets the Authorization header sent upstream.

## Config

| Key                     | Default        | Description                                              |
| ----------------------- | -------------- | -------------------------------------------------------- |
| `port`                  | `8080`         | Router listen port.                                      |
| `backends`              | —              | `{id, baseUrl, weight?, model?, apiKey?, priority?}[]`   |
| `sticky`                | `true`         | Enable conversation pinning.                             |
| `healthCheckIntervalMs` | `10000`        | Health probe cadence.                                    |
| `healthCheckTimeoutMs`  | `2500`         | Probe timeout.                                           |
| `sessionTtlMs`          | `1800000`      | How long a session pin is remembered.                    |
| `upstreamTimeoutMs`     | `600000`       | Per-request upstream timeout.                            |
| `maxRetries`            | `1`            | Failover attempts to other backends.                     |
| `retryStatusCodes`      | `[429, 529]`   | Upstream status codes treated as "limit reached".        |
| `retryOnLimitMessage`   | `true`         | Also fail over on rate-limit/quota wording in the error body. |
| `models`                | proxied        | Static `/v1/models` list (see below).                    |
| `modelMap`              | `{}`           | Client model → backend model rewrites (see below).       |
| `apiKeys`               | `[]`           | Client API keys required on incoming requests.           |

### What is `models` for?

`models` is the list advertised by the router's `GET /v1/models`. Clients like GitHub Copilot
CLI query that endpoint to see what's available and may refuse models not on it. It's a *static
catalog* — set it to whatever model names you want clients to use:

```json
"models": ["gpt-4o", "claude-sonnet-4.5"]
```

It does **not** validate requests: if a client sends a model not on the list, the router still
forwards it and it's up to the backend to accept or reject. Omit `models` entirely and the
router proxies the list from a healthy backend instead.

### What is `modelMap` for?

Backends often expose models under different names. `modelMap` rewrites the model name the
client asked for into whatever the backend actually expects. It's applied on forward, *after*
the per-backend `model` override:

```json
"modelMap": { "gpt-4o": "claude-3-5-sonnet" }   // client says "gpt-4o" -> backend gets "claude-3-5-sonnet"
```

Put a model map on a single backend if only that backend needs the rewrite:

```json
{ "id": "opencode-2", "baseUrl": "http://127.0.0.1:3457/v1", "weight": 1, "model": "claude-3-5-sonnet" }
```

### Primary backend + limit-reached fallback

Give your preferred backend `priority: 0` and the others `priority: 1`:

```json
"backends": [
  { "id": "account-1", "baseUrl": "http://127.0.0.1:3456/v1", "weight": 1, "priority": 0 },
  { "id": "account-2", "baseUrl": "http://127.0.0.1:3457/v1", "weight": 1, "priority": 1 },
  { "id": "account-3", "baseUrl": "http://127.0.0.1:3458/v1", "weight": 1, "priority": 1 }
]
```

account-1 is always tried first by new conversations. If it responds "limit reached" (429, or
an error body with rate-limit/quota wording) the router retries the request on account-2 or
account-3 and **re-pins that conversation to whichever one served it** — so the next turn
reuses that backend's warm cache instead of paying full prefix cost again on account-1. A
fresh conversation still starts on account-1 (it's the priority). Bump `maxRetries` to control
how many fallbacks run per request.

### Backend vs client API keys

- **`apiKey` (per backend)** — the Authorization header sent to that backend, e.g. `"apiKey":
  "opencode-key"` if your opencode instance requires one.
- **`apiKeys` (router-level)** — when set, clients must authenticate to the router with one of
  these keys:

```json
"apiKeys": ["client-key-1", "client-key-2"]
```

Point Copilot CLI at it with `COPILOT_PROVIDER_API_KEY=client-key-1`. When `apiKeys` is
configured, the client's key is **not** forwarded upstream (it's the router's credential); use
per-backend `apiKey` if backends need their own keys. The dashboard (`/` and `/api/status`)
stays open on localhost so you can watch routing.

## Tests

```bash
deno task check   # type-check
deno task test    # unit tests
```

## Quick smoke test without real backends

The built-in sandbox spins up a full mini-cluster — a router plus three fake backends — so you
can watch load balancing, sticky sessions, model validation, and the dashboard without any real
opencode instances:

```bash
aiproxy.bat --sandbox -p 6060   # terminal 1: router on 6060, fake backends on 6061-6063

# terminal 2: point Copilot CLI at the router (one of the sandbox models)
COPILOT_PROVIDER_BASE_URL=http://127.0.0.1:6060/v1 \
COPILOT_PROVIDER_TYPE=openai COPILOT_PROVIDER_API_KEY=x COPILOT_MODEL=sandbox-gpt-4o \
copilot -p "hello" --allow-all-tools --silent
```

Sandbox models: `sandbox-gpt-4o`, `sandbox-claude-sonnet`, `sandbox-llama-3.1-70b`. Any other
model name gets a `404` from the sandbox backends. Each request is logged to the console with
its internal session id and the backend it was routed to, e.g.:

```
[aiproxy] POST /v1/chat/completions session=f:abd1a5b0b6f5300f7c477461 -> sandbox-1 (http://127.0.0.1:6061/v1) model=sandbox-gpt-4o status=200 1ms
```

Open `http://127.0.0.1:6060/` in a browser for the live dashboard (health, per-backend request
counts, and the most recent routes).

To run just a single fake backend (for manually proxying through a router config like
`config.sandbox.json`, which points at `sandbox-1` on 6061):

```bash
PORT=6061 deno run --allow-net --allow-write src\sandbox-server.ts
```

Sandbox requests are also logged to `%TEMP%\aiproxy-sandbox-<port>.log`.
