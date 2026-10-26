# Configuration

The daemon reads one central TOML document. Treat it and every referenced child
provider file as an immutable run revision: write new files, calculate their
SHA-256 digests, then restart the single daemon. Every allocation retains its
resolved profile and provider/catalog/configuration fingerprints for lifecycle
calls and cleanup.

Start from [`examples/central-runtime.toml`](../examples/central-runtime.toml).
It shows control settings, hard limits, policy, defaults, provider references,
all three cloud catalogs, and GitHub/GARM ingress in one synthetic file. Its
paths, hashes, IDs, account facts, images, network values, and pricing facts are
placeholders; replace them with values verified in the real accounts.
[`central.toml`](../examples/central.toml) is only a small normalization
example and has no provider bindings, cloud catalogs, or ingress.

The exported installation JSON Schema describes the central document.
Repository `.outrider.toml` files accept only `version` and `profiles`; their
parser rejects administrator settings before profile normalization.

Run profile normalization with:

```sh
outrider validate --config /etc/outrider/central.toml --profile default
```

This command parses TOML, rejects unknown fields, reads selected native GARM
and provider files, checks provider configuration hashes and supported auth,
then normalizes the profile against central limits. It makes no cloud calls.
It does not prove credentials can authenticate, catalogs match live APIs,
services are reachable, or guests can bootstrap. `serve` connects the catalogs
and validates the full daemon configuration before accepting work.

Central and repository documents accept TOML only. Use `version = 1`; omit an
optional setting instead of assigning `null`. Required workflow and cloud-init
documents keep their YAML syntax; API payloads and journal snapshots keep JSON.

GARM's external-provider `config_file` must point to this same full central
configuration. Create requires ingress allocation and bootstrap timeout
budgets; its HTTP request timeout is the lesser of the allocation budget in
seconds and the bootstrap budget in minutes converted to seconds. The
allocation executor reserves the configured timeout margin within that bound.
Other provider lifecycle commands continue to use
`control.provider_timeout_seconds`.

## Authority and profiles

1. Central limits define hard ceilings and allowed clouds, accounts, locations,
   machine types, images, operating systems, architectures, capabilities, and
   purchasing modes.
2. A repository's `.outrider.toml` can select a named profile and replace
   permitted selector lists. Central limits remain authoritative.
3. A job token selects a profile and supplies only explicitly configured,
   canonical overrides. Repository files cannot set credentials, bindings,
   hard limits, purchasing outside central policy, or policy definitions.
4. GARM pool/bootstrap data supplies request context and internal lifecycle
   identity. Outrider validates and journals it before delegating a mutation.

The central `defaults` block defines profile defaults. Named entries under
`profiles` override those values. Repository profile entries can override only
repository-editable fields; list fields replace the prior list instead of
merging. Tightened current hard limits are rechecked for pending work. Changed
defaults or provider pins do not rewrite an existing allocation's identity.

## Costs and selection

`cost_horizon_seconds` estimates the expected billable allocation duration.
Each profile must resolve a purchasing choice, and central
`default_purchasing` supplies it when no profile or request selects one. The
providers currently expose `on_demand` and `spot` when the binding and live
offer support them. Quotes project the components Outrider can identify:
compute, boot disk, configured public address, and image licensing. Apply
`maximum_total_cost` to that projected total. This is an estimate, not an
all-in cloud bill: AWS excludes tax, negotiated discounts, and data transfer;
Azure excludes per-runner disk transaction charges; GCP excludes network
egress and taxes. Missing required quote data follows `missing_data`: `reject`
declines the offer, while `fallback` tries the named configured policy. A
fallback chain must be valid and acyclic. Unknown or missing charges are never
treated as zero or as covered by the ceiling.

Policies use one of four strategies: `economy` ranks eligible offers by
projected cost; `continuity` ranks observed interruptions per running hour and
definitive allocation failure fraction within configured windows and freshness
limits; `weighted` scores cost, recent create failures, latency, and
interruptions; `ordered` follows configured cloud/account/location/machine and
purchasing order. Eligibility and hard limits run before ranking for every
strategy. Fallback policy references must be valid and acyclic.

Create history contains positive child returns and their measured latency.
Dispatched attempts with no recorded positive return remain unknown, including
opaque child errors and attempts left unresolved at restart. A positive child
return without verified native ownership also leaves outcome coverage unknown.
Unknown coverage makes the create-failure dimension unavailable, so policies
using `continuity` or a positive failure weight follow configured `missing_data`
behavior.
Successful latency and interruption observations remain independently usable
when their dimensions are active. The pinned providers do not currently expose
durable, definitive failed-create outcomes. Zero observed failures is valid
only when all dispatched outcomes in the window are qualified successes;
success-only samples cannot hide unknown outcomes.

## Native providers and catalogs

Set `[garm].config_file` to the absolute native GARM configuration path.
Each `[[providers]]` entry selects its exact native `provider.name` and records
Outrider's hashes, source revision, expected version, OS/architecture support,
and purchasing capabilities. GARM owns executable/configuration paths,
interface versions and environment prefix selectors. See
[`examples/garm.toml`](../examples/garm.toml). Central configuration rejects
copies of those native registration fields.

Native AWS TOML supplies region and subnet; central policy keeps the expected
AWS account ID, pricing geography, availability zone and security groups.
Native Azure TOML supplies subscription, location and network mode/subnet;
its subscription becomes the account identity. Native GCP TOML supplies
project and zone; Outrider derives its pricing region from that zone.
Each `*_catalog_bindings[]` entry references the selected native name through
`provider_binding_id`, then supplies image, storage, licensing, performance
and launch policy facts that native TOML does not contain.

Supported authentication remains AWS static credentials, Azure service
principal, and an explicit absolute GCP service-account key file. Native
credentials stay in protected provider files. Outrider does not copy them
into central configuration, snapshots or parser errors.

Native environment selectors are prefixes, including GARM's empty-prefix
behavior. Outrider expands them once against the daemon's startup environment;
GARM's own environment does not supply missing daemon variables. Snapshots
retain selected variable names only. Restore any required names before
restarting cleanup; missing retained names fail closed. Values stay in memory.

Outrider limits apply to Outrider-managed pools. GARM administrators can create
direct native-provider pools outside Outrider's policy and ownership.

The Linux bootstrap gate
requires the selected image to attest `cloud-init` or `cloud_init` for each
cloud. The AWS adapter separately requires `ec2_launch_agent` for Windows.
Azure and GCP Windows do not use that Linux gate: the Azure child installs a
Custom Script Extension that reads `C:/AzureData/CustomData.bin` and runs it
with PowerShell; the GCP child uses the Windows
`sysprep-specialize-script-ps1` metadata key. Verify the selected image runs
the relevant guest consumer; OS metadata by itself does not prove bootstrap
execution. See the [pinned Azure implementation](https://github.com/kellen-miller/garm-provider-azure/blob/10659879c5d91979c7f077439bd78e17622452a6/internal/spec/spec.go#L42)
and [pinned GCP implementation](https://github.com/kellen-miller/garm-provider-gcp/blob/98a362f5319928099b942a668c5a17a34170066a/internal/client/gcp.go#L39).

Azure `public_ip_sku` may be omitted to disable public IP allocation, or set to
exactly `Standard` with the qualified child revision in the manifest. The child
creates a Regional static IPv4 address when `allocate_public_ip` is selected;
Outrider includes its separate Retail charge. Other SKUs and older child
revisions are rejected. Catalog location and subnet must match the child
configuration and its isolated or shared-network mode.

Rust reads cloud APIs for catalogs, prices, and observations. Pinned GARM
executables perform create, start, stop, delete, and guest bootstrap delivery.
Outrider has no Rust cloud-mutation fallback if a child fails.
