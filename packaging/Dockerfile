# Build context: Dockerfile beside package/, the verified cargo xtask package output.
# Linux amd64 manifest fetched from the authoritative gcr.io registry on 2026-10-26;
# SHA256 of the response body equals its Docker-Content-Digest header.
FROM gcr.io/distroless/static-debian13:nonroot-amd64@sha256:433bb346cd0b6b8346462f879adaa4ac0fbb463a72901b46736ede4aeca3dadc

# The base contains ca-certificates.crt; the product and CGO_ENABLED=0 delegates
# are static Linux amd64 binaries. No compiler/package manager is needed here.
COPY --chown=0:0 package/ /usr/share/outrider/package/
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65532:65532
ENTRYPOINT ["/usr/share/outrider/package/bin/outrider"]
CMD ["serve", "--config", "/etc/outrider/config.toml"]
