# mirror-engine image: multi-stage build, slim runtime with curl for smoke tests.
#
# Base images are pinned to the latest stable releases by tag *and* digest, so a
# rebuilt image is reproducible and no tag can silently move under us.
FROM docker.io/library/rust:1.99.1-slim-trixie@sha256:e6cd1d294a300601eaaa4a294b337e5f0826f427fb2d481e9f76e69e7981e244 AS builder
WORKDIR /src

# Dependency layer: build a stub first so `cargo build` caches all deps.
COPY Cargo.toml ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM docker.io/library/debian:13.7-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/mirror-engine /usr/local/bin/mirror-engine
# JSON logs by default in-cluster (kubectl-friendly, one line per event).
ENV MIRROR_LOG_JSON=1
ENTRYPOINT ["/usr/local/bin/mirror-engine"]
