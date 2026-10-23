# Mirror images: multi-stage build, slim runtime with curl for smoke tests.
#
# One Dockerfile, one binary per image: `--build-arg BIN=<package>` selects
# which workspace binary to build (mainnet-engine, venue-engine, venue-sim and
# cctp-bridge). The runtime entrypoint is a symlink to it.
#
# Base images are pinned to the latest stable releases by tag *and* digest, so a
# rebuilt image is reproducible and no tag can silently move under us.
FROM docker.io/library/rust:1.99.1-slim-trixie@sha256:e6cd1d294a300601eaaa4a294b337e5f0826f427fb2d481e9f76e69e7981e244 AS builder
ARG BIN=mainnet-engine
# Hosts without an IPv6 route (this one): crates.io resolves AAAA first and
# cargo's libcurl fails instantly on the unreachable v6 address. Prefer IPv4.
RUN echo 'precedence ::ffff:0:0/96 100' >> /etc/gai.conf
WORKDIR /src

# Dependency layer: build stub members first so `cargo build` caches all deps.
# Cargo.lock is copied and --locked is used so the image builds the exact
# dependency set pinned in the repo (no silent re-resolution, and a stale lock
# fails the build instead of drifting). Every workspace member needs its
# manifest copied and an empty src entry point created here too.
COPY Cargo.toml Cargo.lock ./
COPY crates/anvil/Cargo.toml crates/anvil/
COPY crates/cctp/Cargo.toml crates/cctp/
COPY crates/config/Cargo.toml crates/config/
COPY crates/devnet/Cargo.toml crates/devnet/
COPY crates/ethereum/Cargo.toml crates/ethereum/
COPY crates/hyperliquid/Cargo.toml crates/hyperliquid/
COPY bins/cctp-bridge/Cargo.toml bins/cctp-bridge/
COPY bins/mainnet-engine/Cargo.toml bins/mainnet-engine/
COPY bins/venue-engine/Cargo.toml bins/venue-engine/
COPY bins/venue-sim/Cargo.toml bins/venue-sim/
RUN for d in crates/anvil crates/cctp crates/config crates/devnet crates/ethereum crates/hyperliquid; do \
      mkdir -p "$d/src" && : > "$d/src/lib.rs"; \
    done \
    && for d in bins/cctp-bridge bins/mainnet-engine bins/venue-engine bins/venue-sim; do \
      mkdir -p "$d/src" && echo 'fn main() {}' > "$d/src/main.rs"; \
    done \
    && cargo build --release --locked -p ${BIN} \
    && rm -rf crates bins

# Source layer: real sources; touch them so cargo does not trust the stub
# artifacts' mtimes (COPY preserves the build context's mtimes).
COPY crates ./crates
COPY bins ./bins
RUN find crates bins -name '*.rs' -exec touch {} + \
    && cargo build --release --locked -p ${BIN}

FROM docker.io/library/debian:13.7-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
ARG BIN=mainnet-engine
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/${BIN} /usr/local/bin/${BIN}
RUN ln -s /usr/local/bin/${BIN} /usr/local/bin/entrypoint
# JSON logs by default in-cluster (kubectl-friendly, one line per event).
ENV MIRROR_LOG_JSON=1
ENTRYPOINT ["/usr/local/bin/entrypoint"]
