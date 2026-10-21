# GARM cross-cloud scheduling evidence

Checked 2026-10-21. Primary sources only. Read-only upstream inspection; no cloud provisioning was tested.

## Stable release and balancing

- Latest non-prerelease: **v0.2.1**, published 2026-05-08, commit `154638445c3949c1958b01812f69d9a1e4d82684`. Verified GitHub release API (`prerelease:false`, `draft:false`) and latest-release redirect. [Release](https://github.com/cloudbase/garm/releases/tag/v0.2.1), [API](https://api.github.com/repos/cloudbase/garm/releases/latest).
- Stable already supports `roundrobin` (default) and `pack`. Pack tries matching pools in descending numeric priority, filling preferred pools before overflow. A pool fixes its provider, image, flavor and extra specs. Thus pools can encode a manually chosen cost ordering across providers. [Tagged docs](https://github.com/cloudbase/garm/blob/v0.2.1/doc/pools-and-scaling.md#pool-balancing), [actual balancer](https://github.com/cloudbase/garm/blob/v0.2.1/runner/pool/cache.go#L46-L74).
- **No native dynamic cheapest cross-cloud Spot selection in the inspected scheduler.** This is a source-based conclusion: scheduler uses labels, configured priority, enabled state and pool capacity; the provider interface has no price/capacity discovery operation. It does not compare live cloud prices, regions, machine families or interruption risk. A price-aware controller/custom provider would be additional software. [Selection loop](https://github.com/cloudbase/garm/blob/v0.2.1/runner/pool/pool.go#L2136-L2187), [pool gate](https://github.com/cloudbase/garm/blob/v0.2.1/runner/pool/pool.go#L1326-L1343), [provider interface](https://github.com/cloudbase/garm/blob/v0.2.1/runner/common/provider.go#L25-L46).
- Main at inspection: `3d907a17018e1cb3056aadabb57cc1158683ec75`. Balancer file identical to stable; pool-balancing documentation semantically identical (formatting/TOC differences). These findings do not depend on unreleased main. [Pinned main balancer](https://github.com/cloudbase/garm/blob/3d907a17018e1cb3056aadabb57cc1158683ec75/runner/pool/cache.go).

## Failure fallback caveat

- The selection loop tries the next pool when the synchronous allocation gate fails, e.g. disabled/full pool or failure registering/persisting a pending runner. **Successful allocation only creates a pending-create database record; it does not prove VM capacity.** Cloud creation runs afterward in a goroutine. Therefore “pack spills over” must not be interpreted as immediate failover on cloud Spot capacity errors. [Pending allocation](https://github.com/cloudbase/garm/blob/v0.2.1/runner/pool/pool.go#L936-L996), [async cloud creation/error status](https://github.com/cloudbase/garm/blob/v0.2.1/runner/pool/pool.go#L1709-L1771).
- Provider creation errors become instance-error state; retry logic cleans and requeues the **same pool**, up to five total create attempts. No provider switch occurs in that retry path. [Attempt limit](https://github.com/cloudbase/garm/blob/v0.2.1/runner/pool/pool.go#L64-L70), [retry lifecycle](https://github.com/cloudbase/garm/blob/v0.2.1/runner/pool/pool.go#L1400-L1486).
- Queued-job code has a ten-minute unlock/reconsideration branch. This is not a prompt fallback guarantee, and pack can prefer the original pool again. Startup/source comments also document missed-webhook risk for zero-idle pools while GARM was down. [Queued-job timing](https://github.com/cloudbase/garm/blob/v0.2.1/runner/pool/pool.go#L2060-L2133).

## Provider Spot support

Distinguish implemented main from proposed changes.

- AWS: main commit `4572ed3b7cb575b1dc9993affcd030f97c77c81e`: `RunInstances` construction has no `InstanceMarketOptions`; inspected config/spec and README expose no Spot setting. This indicates no implemented Spot provisioning option in the checked provider, not a limitation of EC2 itself. [AWS creation code](https://github.com/cloudbase/garm-provider-aws/blob/4572ed3b7cb575b1dc9993affcd030f97c77c81e/internal/client/aws.go), [spec](https://github.com/cloudbase/garm-provider-aws/blob/4572ed3b7cb575b1dc9993affcd030f97c77c81e/internal/spec/spec.go).
- Azure: main commit `0b0effee0f653ac2f93810a6105049355735fa88`: VM properties omit Spot priority, billing profile and eviction policy; inspected config/spec and README expose no Spot option. [Azure VM construction](https://github.com/cloudbase/garm-provider-azure/blob/0b0effee0f653ac2f93810a6105049355735fa88/internal/spec/spec.go).
- GCP: Spot implementation is proposed in **open, unmerged PR #130** (`spot:true`, DELETE termination behavior); do not describe it as released upstream support. Main README exposes no Spot option. [Proposed implementation](https://github.com/cloudbase/garm-provider-gcp/pull/130).

Practical conclusion: GARM supplies multi-provider runner management and static preference/overflow. Dynamic cheapest available Spot placement plus reliable capacity failover requires additional scheduling/provider work and qualification. No deployment or runtime behavior was tested here.

## RunsOn reference: named profiles and allocation policies

Follow-up scope correction: the requested model is reusable named VM profiles plus selectable allocation policy; automatic global cheapest placement is not an inherent requirement.

Profiles: Flex defines `runners.<name>` in `.github/runs-on.yml`, selected by `runner=<name>`. Profiles collect CPU/RAM ranges, instance families, image, storage, Spot policy and extras. Fleet instead defines Terraform-owned `runners` and exposes them through `fleets`; workflows select a fleet rather than override its shape. [Custom runners](https://runs-on.com/docs/runners/custom/).

Flex precedence/configuration: Job labels override built-in/custom runner fields. `extras` replaces the complete list; mandatory stack extras are added back. Local inherited configuration overrides global values without deep merging. `_extends` selects shared definitions, unless the stack auto-extends setting replaces it (default `.github-private`). Public repositories read config from the default branch; private repositories read the current branch. [Labels and configuration](https://runs-on.com/docs/runners/labels/#repository-configuration).

Policies: `spot=true` defaults to `price-capacity-optimized` (`pco`). Alternatives: `lowest-price` (`lp`), `capacity-optimized` (`co`), `capacity-optimized-prioritized` (`cop`, v3.3.0+). `spot=false` selects on-demand. COP accepts exact instance types in preference order, not families/wildcards; capacity takes precedence, so order is best effort. Default PCO ignores list order; on-demand prioritizes exact types automatically. [Spot label reference](https://runs-on.com/docs/runners/labels/#spot).

Fallback: RunsOn documents Spot-first then on-demand fallback when capacity is unavailable (Flex adds 2–3 seconds; current Fleet retries affected runners around five seconds later). From v3.5, Fleet fallback applies only to affected runners, unlike its older fleet-wide five-minute fallback. [Spot behavior](https://runs-on.com/docs/costs/spot-pricing/#default-behavior).

Cross-cloud boundary: The named profile/override interface is portable as a design pattern. These allocation strategy names and semantics are AWS EC2 Fleet/Spot Fleet mechanisms. Supporting other clouds requires explicit provider mappings or controller-owned semantics; do not promise identical native capacity optimization across clouds. [AWS allocation strategies](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-fleet-allocation-strategy.html).
