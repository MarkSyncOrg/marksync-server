# syntax=docker/dockerfile:1

# Build a fully static binary against musl, so the runtime image can be `scratch`.
FROM rust:1-alpine AS build
RUN apk add --no-cache build-base cmake perl
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY config config
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/marksync-server /marksync-server
# Fail the build if anything is dynamically linked.
RUN ! ldd /marksync-server 2>/dev/null | grep -q '=>'
# Filesystem skeleton for scratch: data dir owned by the runtime user, a sticky /tmp for
# SQLite temporary files, and the settings directory.
RUN mkdir -p /rootfs/tmp /rootfs/app/config /data \
    && chmod 1777 /rootfs/tmp

FROM scratch
COPY --from=build /rootfs /
COPY --from=build --chown=65532:65532 /data /data
COPY --from=build /marksync-server /marksync-server
WORKDIR /app
# Container defaults; a mounted /app/config/settings.json overrides them.
ENV MARKSYNC_SETTINGS_JSON='{"server":{"host":"0.0.0.0","port":8080},"db":{"path":"/data/marksync.db"}}'
VOLUME /data
EXPOSE 8080
USER 65532:65532
HEALTHCHECK --interval=1m --timeout=10s --start-period=10s --retries=5 \
    CMD ["/marksync-server", "healthcheck"]
ENTRYPOINT ["/marksync-server"]
CMD ["serve"]
