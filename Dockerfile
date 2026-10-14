# syntax=docker/dockerfile:1

# Alpine's native target is musl, so the binary is statically linked.
FROM rust:1.95-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/vault-mirror /vault-mirror
# An empty directory the unprivileged user owns, to copy into the final image.
RUN mkdir /data

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /vault-mirror /vault-mirror
COPY --from=build --chown=65532:65532 /data /data
USER 65532:65532
ENV BACKUPS_DIR=/data
VOLUME /data
# Reports unhealthy when the last run failed or is older than 26 hours.
HEALTHCHECK --interval=5m --timeout=10s --start-period=1h CMD ["/vault-mirror", "healthcheck"]
ENTRYPOINT ["/vault-mirror"]
