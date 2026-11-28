# Build context: Dockerfile beside package/, the verified cargo xtask package output.
# Linux amd64 manifest fetched from the authoritative gcr.io registry on 2026-10-26;
# SHA256 of the response body equals its Docker-Content-Digest header.
FROM gcr.io/distroless/static-debian13:nonroot-amd64@sha256:89e00a7e78df216bcbf495e7f43135f03ddbd3e98024a4104731872d6c8cf3c6

# The base contains ca-certificates.crt; the product and CGO_ENABLED=0 delegates
# are static Linux amd64 binaries. No compiler/package manager is needed here.
COPY --chown=0:0 package/ /usr/share/outrider/package/
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65532:65532
ENTRYPOINT ["/usr/share/outrider/package/bin/outrider"]
CMD ["serve", "--config", "/etc/outrider/config.toml"]
