# scion-server for the local compose stack (deploy/docker-compose.yml), built
# from this checkout. The released image is deploy/release.Dockerfile.

FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# rust-toolchain.toml names `stable` with components the build does not need;
# the image's own toolchain builds it without a download.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    RUSTUP_TOOLCHAIN=$RUST_VERSION cargo build --release --locked -p scion-server \
    && cp target/release/scion-server /usr/local/bin/

FROM debian:bookworm-slim
# The S3 client loads the system roots even for a plain-http endpoint.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /usr/local/bin/scion-server /usr/local/bin/scion-server
ENTRYPOINT ["scion-server"]
