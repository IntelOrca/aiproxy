# ---- Build stage ------------------------------------------------------------
# Compile aiproxy into a single self-contained binary.
FROM denoland/deno:alpine-2.9.5 AS build

WORKDIR /app

# All imports are local, so copying the sources is enough.
COPY . .

# Type-check before shipping.
RUN deno task check

# Compile the router into a standalone binary with its runtime
# permissions baked in.
RUN deno compile \
      --allow-net \
      --allow-read \
      --allow-env \
      --output aiproxy \
      main.ts

# ---- Runtime stage ----------------------------------------------------------
# The compiled binary is self-contained (Deno bundles the root CA store), so a
# bare scratch image keeps the container as small as possible.
FROM scratch

# Run as non-root (numeric uid works without an /etc/passwd in scratch).
USER 65532:65532

# The router reads config.json from the working directory; provide the example
# config as a sensible default. Mount your own config with:
#   docker run -v "$PWD/config.json:/config.json" ...
COPY --from=build /app/aiproxy /aiproxy
COPY --from=build /app/config.example.json /config.json

EXPOSE 8080

ENTRYPOINT ["/aiproxy"]
