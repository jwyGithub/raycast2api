# ---- build stage ----
# Full `rust` image (not -slim) so the C toolchain that `ring` needs is present.
FROM rust:1 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --locked

# ---- runtime stage ----
FROM debian:bookworm-slim
# ca-certificates: TLS root store for outbound HTTPS to the Raycast backend.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --create-home app
COPY --from=builder /app/target/release/raycast2api /usr/local/bin/raycast2api
USER app
# The gateway reads config.json / store/ relative to its CWD.
WORKDIR /app
# Writable home for the SQLite store (bind-mounted over by compose).
RUN mkdir -p /app/store && chown app:app /app/store
# Default listen address is 0.0.0.0:8787 (override with HOST / PORT env vars).
EXPOSE 8787
ENTRYPOINT ["raycast2api"]
