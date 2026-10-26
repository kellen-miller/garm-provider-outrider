# Outrider add-on package

The Linux amd64 package supplies Outrider and its three pinned native providers
for an existing GARM installation. The same static musl executable runs on a
Linux host and inside GARM's BusyBox container. GARM invokes it as an external
provider; one separate Outrider daemon handles routing and lifecycle state.

Packages contain `bin/outrider`, `providers/garm-provider-{aws,azure,gcp}`,
licenses, example TOML, `packaging/outrider.service`, and
`package-manifest.json`. They do not contain GARM, credentials or cloud catalog
snapshots. Provider configurations remain operator-owned native GARM files.

Preserve the installed executables and configuration versions referenced by
retained allocations. Cleanup needs their exact files, including after an
upgrade. Keep credential/configuration permissions restrictive when installing;
package example files contain no secrets.

## Verify an archive

Obtain the archive and checksum from the same trusted release. Before extraction:

```sh
sha256sum --check outrider-1.2.3-linux-amd64.tar.gz.sha256
mkdir outrider-1.2.3
tar -xzf outrider-1.2.3-linux-amd64.tar.gz -C outrider-1.2.3
outrider-1.2.3/bin/outrider build-info
```

The checksum and manifest detect changed bytes. They do not authenticate their
publisher. Trust the release source and its CI identity/provenance separately.
`build-info` reports embedded `version`, `source_revision`, `host`, and `dirty`.
A package requires a clean full source SHA matching its binary and semver.
`--version` and GARM `GetVersion` report that same embedded version.

Manifest schema 1 contains `product: "outrider"`, `version`, `source_revision`,
`host: "linux/amd64"`, `files`, and `providers`. Each file records a relative
`path`, SHA256 and numeric Unix `mode`: 493 (0755) for executables, 420 (0644)
for other files. The manifest excludes itself from `files`. Provider records
carry `cloud`, `source_revision`, `expected_version`, `binary_path`,
`binary_sha256`, `license_path`, and `license_sha256`.

The exact 15-file manifest set is:

- `bin/outrider`
- `providers/garm-provider-aws`, `providers/garm-provider-azure`, `providers/garm-provider-gcp`
- `licenses/aws-LICENSE`, `licenses/azure-LICENSE`, `licenses/gcp-LICENSE`
- `licenses/Outrider-LICENSE`, `licenses/NOTICE`, `licenses/Apache-2.0.txt`
- `examples/garm.toml`, `examples/config.toml`, `examples/config-minimal.toml`, `examples/repository.toml`
- `packaging/outrider.service`

The systemd template uses `/opt/outrider/bin/outrider serve --config
/etc/outrider/config.toml`, user/group `outrider`, and durable state under
`/var/lib/outrider`. Installation creates its account and protected authored
configuration. The service has no dependency on a particular GARM service name
or a separate token-renewal timer. Public traffic reaches only the signed
webhook endpoint; keep control and GARM administrator APIs private.

## Build from source

Build on native Linux amd64 from a clean checkout with Rust 1.99.1, Go 1.27.3,
`musl-tools`, CMake and clang installed. The provider builder downloads exact
source revisions from `providers/manifest.json`; packaging itself uses local
inputs and performs offline checks.

```sh
rustup target add x86_64-unknown-linux-musl
OUTRIDER_VERSION=1.2.3 CARGO_BUILD_JOBS=2 \
  CARGO_PROFILE_RELEASE_OPT_LEVEL=1 CARGO_PROFILE_RELEASE_LTO=false \
  CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --locked --release --target x86_64-unknown-linux-musl --bin outrider
cargo xtask build-provider-bundle --go-version 1.27.3 --output /tmp/outrider-providers
cargo xtask package \
  --binary target/x86_64-unknown-linux-musl/release/outrider \
  --provider-bundle /tmp/outrider-providers \
  --output /tmp/outrider-package --version 1.2.3
```

`OUTRIDER_VERSION` accepts semver and defaults to the Cargo package version.
Source identity always comes from Git. Debug builds outside Git report unknown
source; such binaries cannot be packaged. Dirty or stale binaries, mismatched
source/version/host, dynamic ELF interpreters/shared libraries, modified
provider binaries/licenses, missing providers, extra bundle files, symlinks and
occupied output directories are rejected. Assembly stages its own files before
renaming the completed directory. A failure leaves existing output untouched.

## CI artifacts and releases

The Package workflow builds native Linux amd64 with the shared Rust action,
release optimization level 1, no LTO and two compiler jobs. It verifies static
ELF layout, embedded identity, package hashes/modes, the systemd unit, and an
offline `GetVersion` invocation inside the pinned GARM 0.2.1 BusyBox image.
These checks exercise installation/runtime compatibility without provisioning
cloud resources.

Pull requests and ordinary pushes upload a seven-day CI archive and checksum.
A CI artifact is not a published release. Only an explicit manual workflow run
on `main` with `release=true` enables publication: the shared semantic-release
action first calculates a version, the package embeds it, then the action
publishes and attaches the matching qualified archive. No release or container
registry writes occur in pull-request jobs. Building this source change does
not publish or deploy anything.
