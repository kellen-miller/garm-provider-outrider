# Add Outrider to an existing GARM container

The supported package and image run on Linux amd64. GARM stays the controller.
Outrider shares its network namespace, native configuration and selected provider
mount paths. GARM invokes `/opt/outrider/bin/outrider` directly as its external
provider; this executable is static musl and also runs in GARM's BusyBox image.
The AWS/Azure/GCP delegates are built with `CGO_ENABLED=0`.

Build from an already verified package, after checking its download checksum and
trusted CI build identity. Its self-authored manifest checks file integrity; it
does not authenticate the publisher. No published image or release is implied by
these commands. No credentials, catalogs, prices or machine snapshots belong in
the image context.

```sh
docker build --platform linux/amd64 \
  --file /absolute/checkout/packaging/Dockerfile \
  --tag outrider:local /absolute/verified-package
docker run --rm --platform linux/amd64 outrider:local build-info
```

The Dockerfile pins the amd64 manifest of the
[official Distroless static Debian 13 image](https://github.com/GoogleContainerTools/distroless).
On 2026-10-26 its authoritative registry returned digest
`sha256:433bb346cd0b6b8346462f879adaa4ac0fbb463a72901b46736ede4aeca3dadc`;
the manifest response body hashes to that value. The referenced configuration
reports `linux/amd64`, and its layer history includes the CA certificate bundle.
`SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt` is available to Outrider and its
Go delegates. The image has no shell, compiler or package manager. Supply any
private CA trust as a protected read-only PEM bundle mounted at that same path in
both GARM and Outrider; delegates executed by GARM use GARM's trust configuration.

Choose the **existing GARM process's numeric UID and GID**. Both its provider
process and the daemon must read the same `0600` control/token files. Native GARM
credentials and cloud credential mounts retain their original modes; align the
process identity and selected credential paths instead of making secrets public.
If existing native files require a different identity/group, resolve that before
starting the addon. Rootless/user-namespace containers must map these IDs and the
protected host bind ownership consistently.

```sh
export OUTRIDER_IMAGE=outrider:local
export OUTRIDER_UID=1000
export OUTRIDER_GID=1000
export OUTRIDER_CONFIG_DIRECTORY=/srv/outrider/config
sudo install -d -m 0750 -o 0 -g "$OUTRIDER_GID" \
  "$OUTRIDER_CONFIG_DIRECTORY"
```

Use the operator's existing Compose project and add
`packaging/compose.yaml` after its current file. It assumes the controller service
is named `garm`. Use the same Compose project name for every command. The host
config directory must already exist, have secure ancestors and contain no
symlinks. Do not place package/config/state under `/tmp`. The init service runs
the product's `install` command without network access. It fills a shared immutable
package volume, initializes the protected state volume and creates a default
config only if absent. The generated config is owned by the installer (root in
this init container), group-readable by the service GID, and mode `0640`.
Its host parent is root-owned `0750` with that same service GID. The init service
safely changes ownership/mode of an empty fresh state
volume to the requested UID/GID and `0700`; existing populated state must already
match. It preserves authored config and the control secret on every repeat.

```sh
export OUTRIDER_COMPOSE=/absolute/checkout/packaging/compose.yaml
docker compose -f compose.yaml -f "$OUTRIDER_COMPOSE" config --quiet
docker compose -f compose.yaml -f "$OUTRIDER_COMPOSE" up outrider-init
sudoedit "$OUTRIDER_CONFIG_DIRECTORY/config.toml"
```

Author the native GARM source path and cloud bindings/profiles/limits in that
config. Set root `webhook_listen_address = "0.0.0.0:8080"` before the TOML tables;
keep `[control].listen_address = "127.0.0.1:9191"`. The daemon's public listener
serves signed GitHub webhooks and health only. Its provider/control endpoints stay
on loopback. Set `[authentication]` to the existing protected GARM administrator
credential **JSON** (`{"username":...,"password":...}`), GitHub App key and
required installation/key identity references. Do not embed secrets in Compose
environment variables. Configure repository webhook secret file references and
operator facts that live APIs cannot supply. `setup` verifies static package pins
and prepares native registration. The running daemon resolves existing identities
in memory and exclusively issues and renews ingress tokens. It does not rewrite
config. Existing credentials/repositories retain their ownership.

The addon inherits GARM's existing volumes read-only at identical absolute paths,
including selected delegate executables/config/credentials. Check the rendered
Compose model for accidental mount hiding. Add explicit matching mounts where a
selected file is baked only into the GARM image, or resides outside its existing
mounts. GARM receives the package/config/state at the same absolute paths. Its
existing DB mount remains unchanged. Outrider's config mount is read-only during normal operation; its state mount is
writable for token renewal. GARM sees both addon mounts read-only.
No Docker socket is mounted.

Start the existing controller if needed, then run setup in its network namespace.
`--register` explicitly adds Outrider's own native registration and preserves
other providers. Grant the one-off command write access to the **directory**
containing the native GARM TOML so atomic publication can work. A read-write bind
of the individual TOML file cannot support atomic replacement. This override is
only needed when choosing `--register`.

```sh
docker compose -f compose.yaml -f "$OUTRIDER_COMPOSE" up -d garm
export GARM_CONFIG_DIRECTORY=/absolute/native/garm/config-directory
docker compose -f compose.yaml -f "$OUTRIDER_COMPOSE" run --rm --no-deps \
  --user 0:0 \
  --volume "$OUTRIDER_CONFIG_DIRECTORY:/etc/outrider" \
  --volume "$GARM_CONFIG_DIRECTORY:$GARM_CONFIG_DIRECTORY" \
  outrider setup --config /etc/outrider/config.toml \
  --package /opt/outrider --register
docker compose -f compose.yaml -f "$OUTRIDER_COMPOSE" up -d outrider
docker compose -f compose.yaml -f "$OUTRIDER_COMPOSE" run --rm --no-deps \
  outrider doctor --config /etc/outrider/config.toml --online
```

`setup` performs local static preparation; it does not issue tokens or qualify
API adoption. The one-off setup command runs as root to preserve root ownership
of config and native registration; token/state ownership remains the daemon's.
`doctor --online` needs the running daemon and current token files; its live
checks do not allocate VMs. Set the reverse proxy's webhook
upstream to GARM's shared namespace on port 8080 and change the existing GitHub
webhook destination to `/v1/webhooks/github`. The fragment defaults its host bind
to `127.0.0.1:8080`. Set `OUTRIDER_WEBHOOK_BIND` only when the reverse proxy requires
a different host interface. Publish this webhook listener on the **GARM** service;
never publish port 9191. If GARM already uses port 8080 inside its namespace,
choose another authored listener port and update the fragment's container port.
An independently networked reverse-proxy container can use `garm:8080` on GARM's
existing Docker network.

For existing **host GARM**, run the same image with `--network host`, mount the
installed prefix/config/state and selected native config/delegates/credentials
at their identical absolute host paths, and set `--user UID:GID` to the aligned
identity. Use `--entrypoint /opt/outrider/bin/outrider`; run `setup` once, then
`serve --config /etc/outrider/config.toml`. Host networking requires a Linux host.
Keep control loopback-only and send only public webhook traffic through the
reverse proxy. No GARM container or controller replacement is needed.

Upgrades use a new version-specific immutable prefix/package volume and native
registration paths. Keep old prefixes while allocations still reference them so
cleanup executes the original delegates. The installer rejects changed packages
at an existing nonempty prefix; there is no force-overwrite option.

## Linux artifact smoke checks

Run against the real CI package, without cloud/GitHub credentials. The provider
entry point and direct GARM-container invocation need the same mounted absolute
config paths for real requests; `GetVersion` below needs none.

```sh
export PACKAGE=/absolute/verified-package
export GARM_IMAGE=your-existing-verified-garm-image-reference
file "$PACKAGE/bin/outrider" "$PACKAGE"/providers/garm-provider-*
readelf -l "$PACKAGE/bin/outrider"
docker build --platform linux/amd64 -f packaging/Dockerfile \
  -t outrider:smoke "$PACKAGE"
docker run --rm --platform linux/amd64 outrider:smoke build-info
docker run --rm --platform linux/amd64 --entrypoint /opt/outrider/bin/outrider \
  -e GARM_COMMAND=GetVersion -v "$PACKAGE:/opt/outrider:ro" "$GARM_IMAGE"
docker compose -f compose.yaml -f packaging/compose.yaml config --quiet
```

Also exercise init installation twice in disposable volumes: confirm the manifest
and binaries are identical, config/control-token inode and bytes are preserved,
state/tokens remain `0700`, tokens remain `0600`, config remains root-owned
`0640` with the service GID, provider GetVersion works
inside GARM, and the public listener returns 404 for private control routes. Use
an authored configuration and local API stubs for the daemon probe; no live
credentials or VM creation are required. CI must run these probes against its
actual musl package; local filesystem fixtures alone cannot qualify Linux runtime
behavior.
