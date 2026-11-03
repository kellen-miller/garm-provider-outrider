# Build context: Dockerfile beside package/, the verified cargo xtask package output.
# Linux amd64 manifest fetched from the authoritative gcr.io registry on 2026-10-26;
# SHA256 of the response body equals its Docker-Content-Digest header.
FROM gcr.io/distroless/static-debian13:nonroot-amd64@sha256:336e53e9941facdb69b755e6fd2c63f5400abd3b1ba47bb17ebafc2b1547cebd

# The base contains ca-certificates.crt; the product and CGO_ENABLED=0 delegates
# are static Linux amd64 binaries. No compiler/package manager is needed here.
COPY --chown=0:0 package/ /usr/share/outrider/package/
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65532:65532
ENTRYPOINT ["/usr/share/outrider/package/bin/outrider"]
CMD ["serve", "--config", "/etc/outrider/config.toml"]
