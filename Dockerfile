FROM rust:1.95.0 as builder

COPY . /browser-headless

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config

RUN cd /browser-headless \
    && cargo build --release


FROM debian:trixie-slim

EXPOSE 3000

# Runtime needs:
#   - chromium: the CDP target chromiumoxide drives
#   - ca-certificates: TLS validation for HTTPS upstream pages
#   - fonts-liberation + fonts-noto-cjk: prevent missing-glyph boxes when
#     rendering Latin / CJK pages (matters for innerText and screenshots)
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        chromium \
        ca-certificates \
        fonts-liberation \
        fonts-noto-cjk \
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

CMD ["browser-headless"]
