# Provider installation

Outrider delegates cloud mutations and guest bootstrap to the pinned GARM AWS,
Azure, and GCP provider executables in [`providers/manifest.json`](../providers/manifest.json).
Build and install those executables for the GARM controller host. Outrider's
central daemon and its child-process supervision currently require a Unix
host; the provider bundle builder supports Linux amd64/arm64, macOS arm64, and
Windows amd64 targets when run on that same host. A target binary is not
interchangeable with a host binary. Runner OS/architecture is selected
separately and described in [limits](limits.md).

Outrider owns the install script for its snapshot-backed pools. After validating
the allocation request, it replaces GARM's incoming `runner_install_template`
with its generated script. Templates manually set on these pools are also
replaced. Pre-install scripts, extra context, and unknown bootstrap options
remain unsupported; configure runners through Outrider profiles instead.

The manifest's `darwin/arm64` hashes identify only observed local artifacts.
They are not Linux or Windows hashes and do not qualify those hosts.

## Build pinned providers

Use Git, Rust (the pinned repository toolchain), and Go 1.27.2. That exact Go version is the original bundle
qualification input. Go 1.26.2 is the minimum used by the tagged GARM source
probe; it does not qualify these provider builds.

```sh
GO=/absolute/path/to/go \
  cargo xtask build-provider-bundle \
    --go-version 1.27.2 \
    --output /var/tmp/outrider-provider-bundle
```

The Rust command places source checkouts and Go build caches in temporary directories
and removes them after completion. Pass `--source-cache /path/to/cache` or
`--go-cache-root /path/to/cache` to retain those inputs explicitly. It checks
the exact Go version, source origin, full manifest commit, upstream license
digest, builds with `-trimpath -buildvcs=false`, then records host, source,
toolchain, binary digest, and `GetVersion` output. Keep the generated bundle
manifest and its `licenses/` directory with the binaries. The bundle records
an offline build/version check only; it does not qualify cloud access or guest
startup.

## Register the external provider

GARM needs a separate control configuration for Outrider's `v0.1.0` external
provider interface. In GARM's `config.toml`, an external provider entry has
these fields (use the actual absolute paths):

```toml
[[provider]]
name = "outrider"
provider_type = "external"
description = "Outrider loopback placement provider"

[provider.external]
interface_version = "v0.1.0"
provider_executable = "/opt/outrider/bin/outrider"
config_file = "/etc/outrider/garm-control.yaml"
environment_variables = []
```

The GARM config parser's executable field selects Outrider. GARM passes
`config_file` as `GARM_PROVIDER_CONFIG_FILE`; Outrider reads its control YAML
there and gets the request connection token from the file named by
`control.auth_token_file`. This is control/loopback configuration, not a
cloud provider config. The file must be a valid Outrider YAML document with
`version: 1` and the complete central `control` and `ingress` settings. Create
uses the lesser of `ingress.allocation_timeout_seconds` and
`ingress.bootstrap_timeout_minutes` converted to seconds as its HTTP request
timeout; the allocation executor applies `ingress.timeout_margin_seconds`
inside that bound. Other lifecycle commands retain
`control.provider_timeout_seconds`. Use the same central file for the daemon
and GARM provider registration. GARM supplies the controller ID to Outrider
on each call.

Each `provider_bindings[]` entry in the central YAML separately pins one
cloud child executable and its cloud-specific configuration file by SHA-256.
GARM does not load those cloud files. Outrider verifies and passes the selected
file to the delegated executable as `GARM_PROVIDER_CONFIG_FILE`. Keep those
files immutable and readable by the daemon account.

Keep the GARM controller ID consistent across the GARM installation,
`ingress.controller_id`, and provider lifecycle requests. Retain the exact
Outrider executable, child executables, provider configs, credentials, and
journal for every nonterminal allocation so cleanup can use the saved identity.
Run one daemon process per local SQLite database and make service restart wait
for the prior process to exit.

## Live setup

Replace all synthetic values in [`central-runtime.yaml`](../examples/central-runtime.yaml),
create real provider configs, calculate their SHA-256 digests, and confirm the
actual accounts, network placements, image capabilities, machine facts,
storage classes, and prices. `outrider validate` checks YAML parsing and
normalization of the selected profile against central limits. It does not
prove the central configuration can serve provider Create requests or verify
binding files, hashes, binaries, cloud credentials, catalogs, ingress
connectivity, or GARM registration. Complete live qualification separately.

## Bounded administrator admission

For a live qualification window, add an administrator-only gate to the central YAML:

```yaml
admission:
  state_file: /var/lib/outrider/admission.json
  max_window_seconds: 1800
```

Omitting `admission` selects normal, unbounded product operation. With the gate
configured, missing, unreadable, invalid, expired, or insecure state closes new
allocations. Daemon restart never reopens a window. Repository configuration
cannot override this setting. The daemon reads the current state file at every
admission boundary; no restart or background timer is required.

The administrator writes this exact version 1 JSON contract, with integer Unix
seconds and no additional fields:

```json
{"version":1,"opened_at_unix":1792760400,"expires_at_unix":1792762200}
```

Admission is open only when `opened_at_unix <= now < expires_at_unix` and the
positive duration is at most `max_window_seconds`. Future opening times are
closed. Explicit closed state is `{"version":1,"opened_at_unix":0,"expires_at_unix":0}`;
removing the file also closes admission. Replace the file atomically with a
same-directory rename. Keep the directory and file owned by root or the daemon's
effective user, without group/world write permission; use directory mode `0700`
and file mode `0600` (or directory `0750` and file `0640` when root grants
the daemon group read access).
Every path component must be a real directory, never a symlink, and satisfy the
same ownership/write checks. Use the canonical absolute path. State must be a
regular file with one hard link, at most 4096 bytes. Non-Unix hosts fail closed
when the gate is configured.

The current administrator gate applies before allocation reservation, after
catalog waits, immediately before dispatch, and at the final child-process
spawn after asynchronous pin/version validation. Retained snapshots cannot
bypass it. Pool creation/enabling and queued Outrider job forwarding are also
gated. Signed deliveries remain durable and retryable while closed; an
administrator closure produces no workflow-author rejection check. Completion,
in-progress, and unrelated webhook forwarding continue. Existing-instance
idempotent create results, reads, list, lifecycle operations, callbacks, and
cleanup remain available after expiry. Closure does not terminate an existing
instance or a create already accepted by a child/cloud. The check is a
point-in-time dispatch boundary, not a cloud transaction: expiry or administrator
closure after the final check cannot recall an operation already dispatched.
