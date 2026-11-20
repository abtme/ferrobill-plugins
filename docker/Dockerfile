# The plugins, one image each, from one shared build stage:
#   docker build -f docker/Dockerfile --target directadmin -t ferrobill-directadmin .
#
# Static (musl, pure-Rust TLS), so each image is `scratch`: the binary and the CA bundle, no
# shell. Runs as 65532 and listens on 8080.
#
# BUILDER_IMAGE: any Debian-based image with rustup; the Rust version comes from
# rust-toolchain.toml.
ARG BUILDER_IMAGE=docker.io/library/rust:1.99.1-trixie

FROM ${BUILDER_IMAGE} AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools ca-certificates \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN rustup toolchain install \
 && cargo build --release --locked --target x86_64-unknown-linux-musl --workspace --bins \
 && mkdir -p /out \
 && for b in ferrobill-directadmin ferrobill-hestiacp ferrobill-plugin-cli; do \
      install -m 0755 target/x86_64-unknown-linux-musl/release/$b /out/$b; \
    done

FROM scratch AS base
ARG VERSION=dev
LABEL org.opencontainers.image.source="https://github.com/abtme/ferrobill-plugins" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}"
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
USER 65532:65532
ENV LISTEN=0.0.0.0:8080
EXPOSE 8080

FROM base AS directadmin
LABEL org.opencontainers.image.title="ferrobill-directadmin" \
      org.opencontainers.image.description="DirectAdmin provisioning plugin for ferrobill"
COPY --from=build /out/ferrobill-directadmin /ferrobill-directadmin
# For trying the plugin by hand: docker exec ... /ferrobill-plugin-cli http://127.0.0.1:8080 manifest
COPY --from=build /out/ferrobill-plugin-cli /ferrobill-plugin-cli
ENTRYPOINT ["/ferrobill-directadmin"]

FROM base AS hestiacp
LABEL org.opencontainers.image.title="ferrobill-hestiacp" \
      org.opencontainers.image.description="HestiaCP provisioning plugin for ferrobill"
COPY --from=build /out/ferrobill-hestiacp /ferrobill-hestiacp
COPY --from=build /out/ferrobill-plugin-cli /ferrobill-plugin-cli
ENTRYPOINT ["/ferrobill-hestiacp"]
