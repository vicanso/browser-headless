FROM rust:1.95.0 as builder

# Build-time system deps. Kept above source COPY so this layer caches
# independently of code edits.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /browser-headless

# Stage 1 — dependency-only build. Cargo.toml + Cargo.lock change rarely
# (only on dep upgrades), so this layer's cache hit rate is high across
# regular source edits. A stub `fn main()` lets cargo resolve + download
# + compile every transitive dependency once; then we delete the stub's
# artifacts so the real source forces a fresh compile of just our crate.
#
# Cargo names crate dirs with underscore (`browser_headless`) but the
# final binary keeps the hyphenated package name (`browser-headless`),
# so both patterns are removed.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src \
              target/release/browser-headless \
              target/release/deps/browser_headless* \
              target/release/.fingerprint/browser-headless* \
              target/release/.fingerprint/browser_headless*

# Stage 2 — real source. With deps cached above, this rebuild only
# compiles our crate (~30s typical) instead of the full 3-5 min cold
# build. Layer invalidates only when src/ contents actually change —
# README / docs / workflow edits no longer trigger a Rust rebuild.
COPY src ./src
RUN cargo build --release


FROM debian:trixie-slim

EXPOSE 3000

# Runtime needs:
#   - chromium: the CDP target chromiumoxide drives
#   - ca-certificates: TLS validation for HTTPS upstream pages
#   - fonts-liberation + fonts-noto-cjk: prevent missing-glyph boxes when
#     rendering Latin / CJK pages (matters for innerText and screenshots)
#   - tini: PID 1 init that reaps zombie/orphan processes. The service
#     constantly spawns + kills chromium (pool recycle / crash respawn), and
#     chromium is multi-process, so without a reaping PID 1 orphaned children
#     would accumulate as zombies and eventually exhaust PIDs. tini also
#     forwards SIGTERM for graceful shutdown.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        chromium \
        ca-certificates \
        fonts-liberation \
        fonts-noto-cjk \
        tini \
    && rm -rf /var/lib/apt/lists/*

# Service account with /bin/false to block interactive login; `-m` still
# creates a writable home dir so chromium's user-data cache (~/.cache/chromium)
# works and `docker exec -it <container> bash` (invoked explicitly) has a HOME.
RUN useradd -r -m -s /bin/false rust

COPY --from=builder --chown=rust:rust --chmod=755 \
    /browser-headless/target/release/browser-headless /usr/local/bin/browser-headless

USER rust

# chromiumoxide looks at $CHROME first; without this it would scan PATH for
# google-chrome / chromium variants (works, but explicit is faster + clearer).
ENV CHROME=/usr/bin/chromium

# Health probe for every mode: serve / all serve /healthz on 3000; worker serves
# it on BROWSER_HEADLESS_HEALTH_PORT (default 3000). `browser-headless
# healthcheck` does an internal HTTP GET and picks the port from
# BROWSER_HEADLESS_MODE, so the image needs no curl/wget. The generous
# start-period covers the initial chromium launch.
HEALTHCHECK --interval=30s --timeout=5s --start-period=40s --retries=3 \
    CMD ["browser-headless", "healthcheck"]

# tini as PID 1: reaps orphaned chromium processes and forwards signals to the
# server (graceful shutdown). `docker run --init` is therefore unnecessary.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["browser-headless"]
