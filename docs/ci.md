# CI and qualification

The pinned shared Rust Clippy gate limits each handwritten Rust file to 750
code lines, excluding blank and comment-only lines, including doc comments.
Code with a trailing comment counts once; comment markers inside literals
remain code. Tests and fixtures use the same limit. Generated workspace
packages require explicit `package.metadata.ci.generated = true`; Outrider
has no generated-package exemption. The existing 250-line function limit
also applies.

Modules follow ownership: cloud adapters separate discovery, catalogs, billing
and native identity; allocation owns selection and runner lifecycle; policy
owns eligibility, ranking and cost facts; journal owns durable state; GitHub
owns authenticated intake and delivery. Domain modules under each integration
target retain its test cases and fixtures. Exact acceptance names include
those module paths.

The required bootstrap contract runs Bash syntax checks, cloud-init schema
validation, PowerShell AST parsing, and native macOS `plutil` validation. The
`check` job runs the full Rust contract suite and all required acceptance cases
on the `macos-26` arm64 runner. It installs Python 3.15.x and Go 1.27.3
explicitly; `cargo xtask install-contract-tools` downloads cloud-init 26.2 and
PowerShell 7.6.7 from pinned upstream artifacts and checks their SHA-256
digests. The plist check uses Apple's `plutil`; a substitute parser or missing
tool is not a pass.

Repository orchestration lives in the lightweight Rust `outrider-xtask`
workspace package. `cargo xtask` runs acceptance, Linux target selection,
provider bundling, validator installation and wrapper-probe builds. Integration
fixtures are native Rust executables. Python is only cloud-init's external
runtime, installed on macOS for schema validation; Linux CI needs no Python.

Formatting, Clippy, and the source-length policy run on Linux. macOS uses
the shared all-target build, then the acceptance checker runs the full suite
once, validates all 93 required outcomes, and runs doctests. Keep the required
`check` job as the macOS gate; Linux cannot run every native bootstrap validator.

The macOS test layout has its own cache key (`macos-tests-v1`). This replaces
the old exact-hit cache, which repeatedly rebuilt SDKs without saving refreshed
artifacts. Verify cache behavior with a second hosted run; a cache hit alone
does not prove Cargo reused compiled dependencies.

Each PowerShell validator uses a cache inside its temporary fixture directory.
Parallel validators therefore avoid the shared startup cache implicated in
[PowerShell issue 26528](https://github.com/PowerShell/PowerShell/issues/26528).

The `Linux host contracts` job runs on Ubuntu 26.04. It uses the pinned shared
Rust action for formatting and Clippy, then `cargo xtask check-linux-contracts` compiles and runs the
library tests and Linux-supported contract targets with locked, all-feature
Cargo commands. The
target set covers configuration and cost rules, child-process isolation and
deadlines, provider protocols, read-only AWS/Azure/GCP behavior, signed ingress,
allocation and recovery, SQLite journal locking and cleanup, and the macOS
capability and cost calculations. Selected `system_contract` cases exercise the
native provider authority, snapshot secrecy, authored-field rejection,
live daemon, authenticated explanation, effective-price winner changes,
retained snapshots, signed cleanup, and the tagged GARM v0.1.0 wrapper process.

Those system cases are selected by exact test name after checking their source
for native-validator dependencies. Linux does not install bootstrap validators
or run `required_tools_no_skip`; the full bootstrap acceptance target remains
qualified by the macOS job. Linux results qualify host lifecycle behavior only
and do not claim the full acceptance suite or all 93 required cases passed on
Linux. The macOS `check` job name remains the required full acceptance gate.

The full acceptance command checks every required case in
`tests/acceptance/cases.json`; missing, ignored, duplicated, or failed cases
are errors. Provider source/bundle checks are separate from CI workflow
publication. `cargo xtask build-provider-bundle --plan` prints exact provider
commits and the selected host without cloning or building. A normal bundle
build checks its exact Go version, source origin, commit, license digest,
provider version, and host-specific binary digest. It does not contact a cloud
account.

`outrider validate --config FILE --profile NAME` parses central TOML, resolves
selected native GARM/provider files, verifies provider configuration hashes
and supported auth, then normalizes the selected profile. It does not prove
cloud authentication, catalog freshness, binary versions, GitHub/GARM
connectivity, or guest execution.

Local tests, synthetic HTTP contracts, source inspection, and offline provider
`GetVersion` checks do not prove hosted CI success, a live cloud allocation,
guest bootstrap, runner registration, interruption handling, or complete
cleanup. The tagged GARM v0.2.1 wrapper probe is a separate runtime gate with
its own source attribution and execution evidence.
