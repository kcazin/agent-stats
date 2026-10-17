# agent-stats: one image for every server role. Default CMD runs the relay; override with `hub serve` for a cloud hub.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN cargo build --release --locked -p agent-stats

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data --no-create-home agent-stats \
 && mkdir -p /data && chown agent-stats /data
COPY --from=build /src/target/release/agent-stats /usr/local/bin/agent-stats
ENV DATA_DIR=/data \
    XDG_STATE_HOME=/data/state \
    AGENT_STATS_CONFIG_DIR=/data/config \
    PORT=8080
# No VOLUME: Railway refuses the instruction; mount a volume at /data (Railway volume, `docker run -v`).
EXPOSE 8080
USER agent-stats
ENTRYPOINT ["agent-stats"]
CMD ["relay", "serve"]
