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
 && for b in ferrobill-cloudflare ferrobill-directadmin ferrobill-domainnameapi ferrobill-enom ferrobill-gandi ferrobill-hestiacp ferrobill-netearthone ferrobill-nominet ferrobill-plugin-cli; do \
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

FROM base AS nominet
LABEL org.opencontainers.image.title="ferrobill-nominet" \
      org.opencontainers.image.description="Nominet (.uk) registrar plugin for ferrobill"
COPY --from=build /out/ferrobill-nominet /ferrobill-nominet
COPY --from=build /out/ferrobill-plugin-cli /ferrobill-plugin-cli
ENTRYPOINT ["/ferrobill-nominet"]

FROM base AS enom
LABEL org.opencontainers.image.title="ferrobill-enom" \
      org.opencontainers.image.description="eNom registrar plugin for ferrobill"
COPY --from=build /out/ferrobill-enom /ferrobill-enom
COPY --from=build /out/ferrobill-plugin-cli /ferrobill-plugin-cli
ENTRYPOINT ["/ferrobill-enom"]

FROM base AS netearthone
LABEL org.opencontainers.image.title="ferrobill-netearthone" \
      org.opencontainers.image.description="NetEarthOne registrar plugin for ferrobill"
COPY --from=build /out/ferrobill-netearthone /ferrobill-netearthone
COPY --from=build /out/ferrobill-plugin-cli /ferrobill-plugin-cli
ENTRYPOINT ["/ferrobill-netearthone"]

FROM base AS gandi
LABEL org.opencontainers.image.title="ferrobill-gandi" \
      org.opencontainers.image.description="Gandi registrar plugin for ferrobill"
COPY --from=build /out/ferrobill-gandi /ferrobill-gandi
COPY --from=build /out/ferrobill-plugin-cli /ferrobill-plugin-cli
ENTRYPOINT ["/ferrobill-gandi"]

FROM base AS domainnameapi
LABEL org.opencontainers.image.title="ferrobill-domainnameapi" \
      org.opencontainers.image.description="Domain Name API registrar plugin for ferrobill"
COPY --from=build /out/ferrobill-domainnameapi /ferrobill-domainnameapi
COPY --from=build /out/ferrobill-plugin-cli /ferrobill-plugin-cli
ENTRYPOINT ["/ferrobill-domainnameapi"]

FROM base AS cloudflare
LABEL org.opencontainers.image.title="ferrobill-cloudflare" \
      org.opencontainers.image.description="Cloudflare Registrar plugin for ferrobill (domains and expiry dates)"
COPY --from=build /out/ferrobill-cloudflare /ferrobill-cloudflare
COPY --from=build /out/ferrobill-plugin-cli /ferrobill-plugin-cli
ENTRYPOINT ["/ferrobill-cloudflare"]
