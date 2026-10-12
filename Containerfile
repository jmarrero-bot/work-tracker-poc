# The hub's release image: the web UI and a release build of the service on
# a distroless runtime, as a non-root user. CI publishes it on `v*` tags (see
# docs/release.md); docs/hub.md says how to run it.

FROM docker.io/library/node:22-trixie-slim AS web
WORKDIR /build/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM docker.io/library/rust:1-slim-trixie AS builder
WORKDIR /build
# Cargo reads every workspace member's manifest, so the GNOME app comes
# along, though only the service is built.
COPY Cargo.toml Cargo.lock ./
COPY gnome ./gnome
COPY src ./src
# rust-embed embeds the web UI into the binary.
COPY --from=web /build/web/dist ./web/dist
RUN cargo build --release --locked -p workboard \
    && mkdir -p /out/workboard

# glibc and libgcc, as the binary needs, and nothing else: no shell and no
# package manager. Its TLS roots are built in (rustls with webpki-roots).
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=builder /build/target/release/workboard /usr/local/bin/workboard
# The data directory, owned by the service's user, so a new named volume
# mounted there starts out as theirs; the service keeps it private (0700).
COPY --from=builder --chown=65532:65532 --chmod=700 /out/workboard /var/lib/workboard
# The default data directory is $XDG_DATA_HOME/workboard, so the CLI finds
# it in `podman exec` without --data-dir.
ENV XDG_DATA_HOME=/var/lib
VOLUME /var/lib/workboard
USER 65532:65532
# The service listens on 127.0.0.1:8421 inside the container only; publish
# it to the host's loopback through pasta (docs/hub.md).
EXPOSE 8421
# An authenticated read through the CLI, which takes the service's token
# from the data directory.
HEALTHCHECK --interval=1m --timeout=10s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/workboard", "connections"]
# The service shuts down gracefully on SIGINT; as PID 1 it ignores SIGTERM.
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/workboard"]
CMD ["serve", "--hub"]
