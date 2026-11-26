# syntax=docker/dockerfile:1
# Minimal image that only publishes Markdown/HTML to Confluence Data Center:
# its entrypoint is `md2epub publish`. Built on scratch: no shell, no package
# manager, no Linux tools. It holds just the static binary, CA certificates
# (TLS to Confluence and remote images), /etc/passwd for the non-root user,
# and an empty /tmp.
#
#   docker run --rm -u "$(id -u):$(id -g)" -v "$PWD:/work" -e MD2EPUB_CONFLUENCE_TOKEN \
#     kdrakop/md2epub doc.md --url https://confluence.example.com --space DOCS

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/md2epub /md2epub \
 && strip /md2epub
RUN mkdir -p /rootfs/tmp /rootfs/work \
 && chmod 1777 /rootfs/tmp \
 && echo 'md2epub:x:65532:65532::/work:/nonexistent' > /rootfs/passwd

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /rootfs/passwd /etc/passwd
COPY --from=build --chown=65532:65532 /rootfs/tmp /tmp
COPY --from=build --chown=65532:65532 /rootfs/work /work
COPY --from=build /md2epub /md2epub
ENV TMPDIR=/tmp \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65532:65532
WORKDIR /work
ENTRYPOINT ["/md2epub", "publish"]
CMD ["--help"]
