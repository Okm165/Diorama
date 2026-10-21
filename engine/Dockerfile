# mirror-engine image: multi-stage build, slim runtime with curl for smoke tests.
FROM docker.io/library/rust:1.99-slim-bookworm AS builder
WORKDIR /src

# Dependency layer: build a stub first so `cargo build` caches all deps.
COPY Cargo.toml ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM docker.io/library/debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/mirror-engine /usr/local/bin/mirror-engine
# JSON logs by default in-cluster (kubectl-friendly, one line per event).
ENV MIRROR_LOG_JSON=1
ENTRYPOINT ["/usr/local/bin/mirror-engine"]
