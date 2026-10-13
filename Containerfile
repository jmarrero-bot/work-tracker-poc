# The hub's release image: the web UI and a release build of the service on
# a distroless runtime, as a non-root user. CI publishes it on `v*` tags (see
# docs/release.md); docs/hub.md says how to run it. The base images are
# pinned by digest, so a tag rebuilds the same way; docs/release.md says how
# to move them.

FROM docker.io/library/node:22-trixie-slim@sha256:154ba2f4d6fec323d28e4f4bb86bba4677f1223391a1979cf521304e03a98dfa AS web
WORKDIR /build/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM docker.io/library/rust:1-slim-trixie@sha256:2752b332db73fdbb7dc576f06c82ed1f312005784ef913d7e04a28f5f55dc581 AS builder
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
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2
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
# An authenticated read through the CLI, which reaches the service through
# its private socket, in the data directory.
HEALTHCHECK --interval=1m --timeout=10s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/workboard", "connections"]
ENTRYPOINT ["/usr/local/bin/workboard"]
CMD ["serve", "--hub"]
