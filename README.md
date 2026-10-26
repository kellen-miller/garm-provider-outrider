# Outrider

Outrider selects runner placements across AWS, Azure, and GCP for GARM. Rust
owns configuration, catalogs, prices, policy selection, and durable allocation
records. The pinned GARM provider executables own every cloud mutation and guest
bootstrap operation.

Outrider is a loopback daemon and a GARM external provider executable. It is
intended for a single controller host with durable local storage. It does not
make a single SQLite database safe for multiple active daemon processes.

## Install

1. Obtain a verified [Linux amd64 package](docs/install-package.md) or use the
   [container setup](docs/install-container.md). CI produces archives and
   checksums; public release publication is a separate workflow operation.
   Deployment hosts need neither Rust nor Go.
2. Run the supplied `outrider install` with your existing service UID/GID.
   Reuse native GARM provider configuration. If existing executables differ
   from the supported package, select the bundled qualified binaries in GARM.
   Outrider never silently replaces another provider's registration.
3. Edit `/etc/outrider/config.toml`: select native provider names, profiles,
   limits, image/network facts, repository names and protected credential
   references. Live APIs supply catalog/pricing data; supply facts they cannot
   expose explicitly. No catalog snapshots are bundled.
4. Run `outrider setup --config /etc/outrider/config.toml --package /opt/outrider
   --register`. It computes artifact/configuration hashes and prepares only
   Outrider's own native GARM registration. It creates no cloud resources.
5. Start one `outrider serve` process using systemd or the supplied container
   assets. The daemon issues and renews its API tokens, verifies existing
   GARM/GitHub repository identities, and keeps durable SQLite state.
6. Run `outrider doctor --config /etc/outrider/config.toml --online`, then route
   signed `workflow_job` and `workflow_run` events to `/v1/webhooks/github`.
   Keep control/GARM administration private. See [operations](docs/operations.md)
   for App permissions, routing labels and live qualification.

The examples use fake account IDs, images, file paths, hashes, and credentials.
They do not contain production pricing facts or qualify cloud access.
`examples/config-minimal.toml` is a small profile-normalization example;
`examples/config.toml` shows the complete configuration shape.
`setup` fills qualification fields; `doctor` distinguishes static configuration,
API/price checks and actual runner execution. Retain immutable provider/config
files and the journal until every allocation using them has finished cleanup.

## Request flow

```text
signed GitHub workflow_job event
  -> Outrider verifies and journals the event
  -> repository config is read and frozen with an exact GARM pool snapshot
  -> the original signed event is relayed to GARM
  -> GARM invokes Outrider with its internal request token and pool labels
  -> Outrider discovers and prices eligible offers, applies policy and limits
  -> the selected pinned GARM provider executable performs cloud operations
  -> Outrider retains the selected provider, config and native identity
  -> GARM lifecycle calls and recovery use that same retained provider
```

The request token carries a repository ID, run ID, attempt, profile, and
allowlisted overrides. It is an identifier, not a secret. The internal GARM
labels are accepted only in the GARM request path and are never copied from an
arbitrary webhook label list.

## Configuration and policy

Central configuration owns hard resource, account, location, image, platform,
allocation, and total-cost limits. Repository files can select profiles and
replace permitted selector lists; they cannot relax central limits or choose
provider credentials. Job overrides are canonical, explicitly allowlisted,
and are normalized through the same profile path. An administrator must opt in
to fork workflows; forks are denied by default.

The cost horizon is the estimated allocation duration used to compare offers.
Quotes include compute, purchasing mode, disks, public IPs, image licensing,
and other applicable components. Stale or missing price data follows the
selected policy's explicit `reject`, `fallback`, or configured alternate
behavior; a missing quote is never treated as zero. `economy` ranks by total
cost, `continuity` ranks observed interruptions per running hour and then
definitive allocation-failure fraction, `weighted` combines configured cost,
failure, latency, and interruption evidence, and `ordered` follows the
declared provider order. Continuity uses configured observation windows,
sample minimums, and freshness limits. All policies still apply the same
eligibility and hard-limit checks.

See [installation](docs/install.md), [configuration](docs/configuration.md),
[operations](docs/operations.md), the [capability matrix](docs/limits.md),
and [CI qualification](docs/ci.md) before enabling a repository.

## Qualification limits

The provider pins and binary hashes in the manifest describe immutable source
and local `GetVersion` artifacts for the stated host. They do not establish a
live cloud allocation, runner registration, guest bootstrap, interruption
handling, hosted CI success, or complete cleanup. Synthetic HTTP fixtures and
local child processes qualify bounded interfaces only. The separately executed
tagged GARM wrapper probe and full controller/cloud/guest qualification must be
reported with their own evidence.

macOS runner allocation is unsupported until GARM, common bootstrap, and the
AWS provider all support the required lifecycle. Standalone macOS bootstrap
rendering does not imply allocation support.

## Development

CI uses the pinned shared Rust formatter and Clippy policy. Run the acceptance
target appropriate to the change with
`cargo xtask check-acceptance --milestone <name>`. The full integration
and native validator requirements are documented in [CI](docs/ci.md).

## License

Outrider is source-available under the PolyForm Perimeter License 1.0.0.
Use, modification, and redistribution are permitted subject to its terms;
providing a competing product, including one offered free of charge, is
prohibited. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
