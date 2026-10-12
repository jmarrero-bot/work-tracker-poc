FROM docker.io/library/node:22-bookworm AS web
WORKDIR /build/web
COPY web/package*.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM docker.io/library/rust:slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY --from=web /build/web/dist ./web/dist
RUN cargo build --release --locked

FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/workboard /usr/local/bin/workboard
USER 1000:1000
ENTRYPOINT ["workboard"]
CMD ["--data-dir", "/data", "serve"]
