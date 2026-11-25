# ---- fixed Node runtime and transport verification ----
FROM debian:bookworm-slim AS node_transport
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl xz-utils openssl libstdc++6 libatomic1 \
    && rm -rf /var/lib/apt/lists/*
COPY transport/node/install-runtime.sh /tmp/install-runtime.sh
RUN sh /tmp/install-runtime.sh /opt/node "$TARGETARCH"
ENV PATH="/opt/node/bin:$PATH" NODE_TLS_REJECT_UNAUTHORIZED=1
WORKDIR /app/transport/node
COPY transport/node/package.json transport/node/package-lock.json ./
RUN npm ci --ignore-scripts --no-audit --no-fund
COPY transport/node/ ./
# Fail this architecture's build if its real TLS profile or transport differs.
RUN node fingerprint.mjs && node --test test/*.test.mjs

# ---- Rust build stage ----
# Full `rust` image (not -slim) so the C toolchain that `ring` needs is present.
FROM rust:1 AS builder
WORKDIR /app
COPY . .
COPY --from=node_transport /opt/node /opt/node
COPY --from=node_transport /app/transport/node /app/transport/node
ENV PATH="/opt/node/bin:$PATH" NODE_TLS_REJECT_UNAUTHORIZED=1
RUN cargo test --locked --quiet \
    && RAYCAST_TEST_NODE_PATH=/opt/node/bin/node cargo test --locked --lib raycast::node_transport::tests -- --ignored
RUN cargo build --release --locked

# ---- runtime stage ----
FROM debian:bookworm-slim
# ca-certificates: TLS root store for outbound HTTPS to the Raycast backend.
# curl: local HTTP probe used by the Compose healthcheck.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libstdc++6 libatomic1 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --create-home app
COPY --from=builder /app/target/release/raycast2api /usr/local/bin/raycast2api
COPY --from=node_transport /opt/node /opt/node
COPY --from=node_transport /app/transport/node /app/transport/node
ENV PATH="/opt/node/bin:$PATH"
# The gateway reads config.json / store/ relative to its CWD.
WORKDIR /app
# Writable home for the SQLite store (bind-mounted over by compose).
RUN mkdir -p /app/store && chown app:app /app/store
USER app
# Verify the packaged runtime and source remain readable by the runtime user.
RUN node transport/node/fingerprint.mjs
# Default listen address is 0.0.0.0:8787 (override with HOST / PORT env vars).
EXPOSE 8787
ENTRYPOINT ["raycast2api"]
