# Current capabilities and limits

## Runner capability matrix

These are source-level request paths. A row means Outrider can consider that
runner platform when the selected provider binding, catalog, image, network,
shape, and live account all support it. It does not promise that every cloud
location or machine type supplies the combination.

| Runner OS / architecture | AWS | Azure | GCP | Current gate |
| --- | --- | --- | --- | --- |
| Linux / amd64 | Supported | Supported | Supported | Binding and image must attest the platform; bootstrap hook must be executable by the image. |
| Linux / arm64 | Supported | Supported | Supported | Same gates, plus a matching provider binding and eligible machine/image. |
| Windows / amd64 | Supported | Supported | Supported | AWS image must attest a working EC2Launch agent; Azure/GCP use their pinned Windows bootstrap consumers. |
| macOS / arm64 | Upstream-gated | No current catalog path | No current catalog path | Outrider rejects macOS allocation. AWS additionally needs GARM, common bootstrap, and AWS provider lifecycle support. |

The daemon host is a separate platform choice. Its child-process supervision
currently requires Unix. The provider bundle script builds host-matched
executables for Linux amd64/arm64, macOS arm64, or Windows amd64 when run on
the same host. The local manifest's observed binary hashes are only for
macOS arm64. A Windows runner is not a Windows daemon requirement, and a
Windows-targeted child bundle cannot run on a Linux controller.

Provider bindings must explicitly list supported runner OS and architecture;
purchasing is separately limited to `on_demand` and `spot` per binding. A
missing binding, purchasing mode, catalog fact, image capability, or provider
operation fails explicitly. Inventory and prices vary by account, region,
shape, image, and time. Validate actual offerings before enabling selectors.

Central `max_allocations_controller`, `max_allocations_account`, and
`max_allocations_pool` cap active allocations. Ingress also bounds dynamic
pools, runners per pool, pool lifetime, and webhook body size. Repository
configuration selects a profile and permitted selectors; job tokens can set
only centrally allowlisted overrides. These limits govern Outrider admission,
not a cloud provider's final bill.

## Cloud-specific constraints

These are the current adapter boundaries, not a promise that every shape or
image works. Compare them with the pinned [AWS](https://github.com/kellen-miller/garm-provider-aws/tree/3c0e79dddd58ea04b95b163014061dd240d52308),
[Azure](https://github.com/kellen-miller/garm-provider-azure/tree/10659879c5d91979c7f077439bd78e17622452a6),
and [GCP](https://github.com/kellen-miller/garm-provider-gcp/tree/98a362f5319928099b942a668c5a17a34170066a)
provider revisions recorded in the manifest.

| Cloud | Purchasing and credentials | Placement and network | Storage, GPU, and image constraints |
| --- | --- | --- | --- |
| AWS | `on_demand` and `spot`; Outrider catalog reads require static credentials in the pinned provider config. Role credentials are not verified by this adapter. | One configured region; each location alias maps to an AZ, subnet, and at least one security group. | Boot disks are gp3 only. GPU facts come from EC2 shape inventory. Images need explicit OS/architecture and Windows licensing facts; Linux bootstrap requires `cloud_init`, and Windows needs a functioning EC2Launch agent (`ec2_launch_agent`). |
| Azure | `on_demand` and `spot`; authentication currently requires a service principal. Workload/managed identity and custom identity options are rejected. | Catalog entries must match the child provider's configured location and network mode. Public IP allocation supports exact SKU `Standard` only, using the pinned child's Regional static IPv4 contract and a separately priced Retail meter. | Managed boot disks support Standard_LRS, StandardSSD_LRS, or Premium_LRS; ephemeral OS disks are unsupported. Image references need an immutable publisher:offer:SKU:version, and Marketplace plans are rejected. Licensing and GPU facts require operator-supplied provenance. |
| GCP | `on_demand` and `spot`; authentication requires an explicitly configured service-account key file. | Profile location selects the pricing region; the catalog and child remain pinned to one zone in that region. Regional placement is rejected. The child config must name a network and subnetwork. | Boot disk class needs a matching storage class, billing SKU, and performance facts; GPU offers need exact accelerator and billing facts. Image license facts must be explicit. Enabling external IPv4 requires a bound billing SKU. |

For example, a GCP profile can select pricing region `us-central1` while its
catalog and child provider stay pinned to zone `us-central1-a`. These values
serve different purposes; the zone is not the profile location selector.

Bootstrap consumers differ by guest OS. The Linux hook path requires a
catalog `cloud-init`/`cloud_init` attestation for all three clouds. Windows
does not use that Linux gate: the Azure child uses Custom Script Extension to
run the script from `C:/AzureData/CustomData.bin`, while GCP configures
`sysprep-specialize-script-ps1`. AWS Windows requires the image's EC2Launch
consumer. Verify the selected guest image actually runs the corresponding
mechanism; OS metadata alone is not that verification.

## Operational limits

- One active daemon owns one local SQLite journal. Shared-filesystem and
  multi-process ownership are unsupported.
- A dispatched create can have an unknown outcome after timeout, cancellation,
  malformed response, or connection failure. The journal keeps a fence against
  speculative duplicate creation.
- Pinned GARM lifecycle mutations address instances by runner name. A read-only
  ownership check can race with that mutation; keep names reserved through
  cleanup.
- Missing or stale pricing follows the configured missing-data policy and
  cannot be treated as free capacity.
- Retained allocations require the exact provider executable, config,
  credentials, and account access to remain available for cleanup.
- Synthetic HTTP fixtures, local builds, and image metadata do not prove live
  cloud access, VM creation, guest bootstrap, runner registration, interruption
  handling, or complete cleanup.
- macOS bootstrap rendering checks only the rendered payload. They do not
  qualify macOS runner allocation.
