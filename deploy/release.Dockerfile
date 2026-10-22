# The released scion-server image, built by .github/workflows/release.yml from
# the static musl binary in the release archives; nothing is compiled here.
# The context holds `amd64/scion-server` and `arm64/scion-server`, and buildx
# picks one per platform. The base has CA roots (the S3 client loads them even
# for a plain-http endpoint) and a non-root user, and nothing else: no shell.
# The compose development stack builds deploy/scion-server.Dockerfile instead.

FROM gcr.io/distroless/static-debian12:nonroot
ARG TARGETARCH
COPY ${TARGETARCH}/scion-server /usr/local/bin/scion-server
USER nonroot
EXPOSE 7420 8080
ENTRYPOINT ["scion-server"]
