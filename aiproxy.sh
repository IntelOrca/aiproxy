#!/bin/sh
# aiproxy launcher for Linux/macOS: uses the compiled binary when present,
# otherwise builds and runs it with cargo.
set -e
cd "$(dirname "$0")"

BIN="target/release/aiproxy"
if [ -x "$BIN" ]; then
  aiproxy() { exec "./$BIN" "$@"; }
else
  aiproxy() { exec cargo run --release --quiet -- "$@"; }
fi

usage() {
  cat <<'EOF'
Usage: aiproxy.sh [options]

  -p, --port <port>  Listen port.
                     Router: overrides config "port". Sandbox: default 6060.
  -b, --bind <addr>  Listen address: HOST, HOST:PORT, or *:PORT for every
                     interface. Overrides config "host". A port here must not
                     disagree with -p.
  --sandbox          Run the fake sandbox cluster (router + 3 fake backends).
  --config <path>    Router config file (default: config.json).
  -h, --help         Show this help.

Examples:
  ./aiproxy.sh --sandbox -p 6060         Router on 6060 + fake backends on 6061..6063
  ./aiproxy.sh -b '*:9090'               Router on every interface, port 9090
  ./aiproxy.sh -b localhost              Router on localhost, config port
  ./aiproxy.sh                           Router with config.json
  ./aiproxy.sh --config config.alt.json  Router with another config
EOF
}

CONFIG="${CONFIG:-}"
PORT="${PORT:-}"
BIND="${BIND:-}"
SANDBOX=0
while [ $# -gt 0 ]; do
  case "$1" in
    -p|--port)
      PORT="$2"; shift 2 ;;
    -b|--bind)
      BIND="$2"; shift 2 ;;
    --sandbox)
      SANDBOX=1; shift ;;
    --config)
      CONFIG="$2"; shift 2 ;;
    -h|--help)
      usage
      exit 0 ;;
    *)
      echo "Unknown argument: $1" >&2
      echo "Run aiproxy.sh --help for usage." >&2
      exit 1 ;;
  esac
done

if [ "$SANDBOX" = "1" ]; then
  PORT="${PORT:-6060}"
  echo "[aiproxy] sandbox cluster: router on $PORT, fake backends on $((PORT+1)), $((PORT+2)), $((PORT+3))"
  if [ -n "$BIND" ]; then
    PORT="$PORT" aiproxy --sandbox --bind "$BIND"
  else
    PORT="$PORT" aiproxy --sandbox
  fi
fi

if [ -n "$CONFIG" ] && [ -n "$PORT" ]; then
  aiproxy --config "$CONFIG" --port "$PORT"
elif [ -n "$CONFIG" ]; then
  aiproxy --config "$CONFIG"
elif [ -n "$PORT" ]; then
  aiproxy --port "$PORT"
else
  aiproxy
fi
