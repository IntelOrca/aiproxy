# ---- Build stage ------------------------------------------------------------
# Compile a statically linked Rust binary. rustls keeps the binary free of
# OpenSSL, so the runtime stage needs no shared libraries.
FROM rust:alpine AS build

RUN apk add --no-cache musl-dev

WORKDIR /app

# Fetch dependencies first so they can be cached across source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs \
    && cargo fetch --locked

COPY . .

# Format, lint and test before shipping.
RUN cargo fmt --check \
    && cargo clippy --all-targets -- -D warnings \
    && cargo test --locked

RUN cargo build --release --locked --target x86_64-unknown-linux-musl

# ---- Runtime stage ----------------------------------------------------------
# The binary is static, so a bare scratch image keeps the container as small
# as possible.
FROM scratch

# Run as non-root (numeric uid works without an /etc/passwd in scratch).
USER 65532:65532

# The router reads config.json from the working directory; provide the example
# config as a sensible default. Mount your own config with:
#   docker run -v "$PWD/config.json:/config.json" ...
COPY --from=build /app/target/x86_64-unknown-linux-musl/release/aiproxy /aiproxy
COPY --from=build /app/config.example.json /config.json

EXPOSE 8080

ENTRYPOINT ["/aiproxy"]
