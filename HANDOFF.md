# Outrider implementation handoff

Prepared 2026-10-21. Start here when continuing in this repository.

## Immediate request and current state

The user requested a new public repository under their personal GitHub account,
then a handoff document so they can open a new agent directly in that repository.
They previously invoked the **grill-plan-build** workflow to implement Outrider.
This session stops after repository setup and documentation; the new agent owns
the remaining design interview, planning, and implementation.

Repository: <https://github.com/kellen-miller/garm-provider-outrider>

There is no product code, cloud deployment, completed design interview, ExecPlan,
task graph, or activated execution Goal. Only bootstrap documentation exists.
No cloud accounts, credentials, billing resources, VMs, GARM installations, or
GitHub Actions runner registrations were changed.

## Confirmed user decisions

- Project name: **Outrider**. The user accepted the name after it was proposed.
- Repository: **kellen-miller/garm-provider-outrider**, **public**. The user
  explicitly selected their personal account.
- Build a reusable custom GARM provider for VM selection and provisioning across
  clouds. AWS, Azure, and GCP were discussed explicitly; broader GARM provider
  coverage is an ambition whose first-release scope remains unresolved.
- Users define reusable, named runner profiles. A requested profile establishes
  VM requirements and selection criteria.
- Cost optimization is configurable. Outrider should also support other
  selection criteria; it must not be inherently restricted to cheapest placement.
- Spot is optional. Purchasing policy must be configurable to permit or require
  the intended purchasing options, including on-demand.
- RunsOn is a reference for named profiles and selectable allocation behavior.
  The user explicitly rejected copying its strategy names. Use original
  vocabulary and define the behavior independently.
- Continue with grill-plan-build in the new repository. The user's latest
  instruction limits this session to setup and handoff.

The user's description of the desired product:

> I want to build, like, a configuration like that where you can build some
> profiles and, you know, name them just like you have, and then the cost
> selector, or the VM selector, or provider, or whatever we're gonna call it,
> selects based on whatever the selection criteria is.

## Proposals that are not approved decisions

The previous agent suggested these strategy names. The user has not accepted
the individual names or their semantics:

| Candidate name | Proposed intent |
| --- | --- |
| `economy` | Minimize estimated VM cost |
| `availability` | Favor allocation success and lower interruption risk |
| `balanced` | Weigh cost against availability |
| `ordered` | Follow configured preferences |

Other suggestions included resource ranges, image aliases, cloud/region
allowlists, price ceilings, Spot-first/on-demand fallback, and allocation
deadlines. Their exact fields, defaults, precedence, and behavior are unresolved.
Prior YAML examples were illustrations, not an approved configuration contract.

No implementation language, deployment target, cloud adapter scope,
pricing source, or performance objective has been chosen. Go may fit the GARM
ecosystem; that is a recommendation to assess, not a user decision.

## Working architecture to investigate

Register Outrider as a single external GARM provider. Keep allocation policy
inside the provider so selection and cloud provisioning can handle capacity
failures together. A separate always-on pricing service is optional, not a
requirement.

```text
Named profile
  -> validate and resolve requirements, purchasing policy, selection strategy
  -> discover/filter eligible cloud candidates
  -> select or rank candidates according to policy
  -> attempt VM allocation through the relevant cloud adapter
  -> preserve GARM bootstrap configuration and return the provider instance
  -> inspect, list, recover, and delete it through the same provider
```

Separate these domain concepts in the configuration:

1. Requirements cover OS/image, architecture, CPU, memory, disk, capabilities,
   allowed clouds/accounts/regions, and other hard constraints.
2. Purchasing policy specifies allowed purchasing options, preferences, and
   whether/when fallback is permitted.
3. Allocation strategy determines how eligible candidates are selected using
   cost, availability, or explicit preference criteria.

The allocator must preserve hard requirements under every strategy and fallback.
A larger VM, another architecture, another region, or on-demand pricing is not
automatically acceptable merely because provisioning failed elsewhere.

Whether adapters invoke existing provider executables, reuse upstream code, or
call cloud SDKs directly is unresolved. Current upstream Spot gaps mean wrapping
the binaries alone will not supply the requested purchasing behavior.

## Verified integration facts

Full citations and inspected upstream revisions are in
[the research note](docs/research/garm-and-runson.md). Findings describe the
2026-10-21 inspection and require rechecking before implementation choices that
depend on current upstream state.

### GARM

- Latest stable inspected: **v0.2.1**. Prefer tagged contracts over assuming
  documentation on `main` matches a release.
- One controller can use multiple providers. Ordinary pools fix a provider,
  flavor, image, labels, extra specs, and scaling configuration.
- Pool scheduling supports `roundrobin` and static-priority `pack`; it does not
  discover VM catalogs or perform cross-cloud price/capacity optimization.
- The external provider receives operations through environment variables.
  `CreateInstance` receives bootstrap JSON on stdin and returns instance JSON
  on stdout. Inspection, listing, deletion, start/stop, and recovery need real
  implementations with the expected lifecycle semantics.
- Extra specs can carry provider-specific configuration. A possible profile
  mapping is one GARM pool per named profile, using a profile identifier in
  flavor or extra specs. Choose and verify the mapping during design.
- GARM's ordinary pool matcher uses configured labels. An external provider
  does not automatically receive arbitrary per-job configuration overrides.
  RunsOn-style dynamic labels would need a separate, verified integration path;
  do not promise them from a provider alone.
- GARM first records a pending runner, then provisions asynchronously. Pack
  overflow is not immediate cross-cloud failover on a later capacity error.
  Failed creates retry within the same pool. Handling candidate fallback inside
  Outrider avoids relying on pool balancing for that behavior.

### Provider purchasing support

- Inspected AWS upstream creation code did not configure Spot market options.
- Inspected Azure upstream VM properties did not configure Spot priority,
  billing profile, or eviction policy.
- GCP Spot support was an **open, unmerged PR #130** when checked. Its proposed
  `spot: true` scheduling deletes preempted instances.

These are limitations of the inspected GARM providers, not of the clouds.
Do not assume any cloud adapter is production-ready without qualification.

### RunsOn reference

- Flex uses named profiles in `.github/runs-on.yml` and permits per-job label
  overrides. Fleet uses a Terraform-owned runner catalog selected by labels.
- Its documented Spot strategies are `lowest-price`,
  `price-capacity-optimized`, `capacity-optimized`, and
  `capacity-optimized-prioritized`. The default is price/capacity optimization
  with Spot-first and on-demand fallback. These are reference facts, not names
  or defaults approved for Outrider.
- AWS's native allocation strategies use AWS-specific capacity information.
  They do not yield a universal capacity score for ranking AWS, Azure, and GCP.
  Cross-cloud availability policy needs explicit evidence, semantics, and
  behavior when a cloud cannot supply the required signals.

## Open decisions for the next interview

Resolve these with the user before locking the implementation plan. Investigate
facts first; ask for product judgments rather than information available in
upstream source.

1. Should the first release support AWS/Azure/GCP together, one initial adapter
   with a verified extension path, or more GARM-supported clouds?
2. Should configuration be central, repository-owned, or both? Are per-job
   overrides required? Which constraints remain administrator-controlled, and
   what is the precedence?
3. What are the strategy names, objectives, tie-breaking rules, and supported
   signals? What happens with stale/missing data or unsupported combinations?
4. Which purchasing options are allowed or preferred? Define cheapest across
   options, fallback triggers, maximum cost, and deadlines.
5. What belongs in the first profile contract: resource ranges, exact sizes,
   image mapping, architecture, regions/accounts, GPUs, Docker, local disk,
   and other required capabilities?
6. How will durable placement identity, retries, idempotency, orphan
   reconciliation, ambiguous responses, and controller restarts work?
7. Does recovery replace VMs only, or also rerun interrupted GitHub workflows?
   These are separate behaviors with different permissions.
8. Which language, supported GARM release(s), verification approach,
   and live-cloud qualification budget should the project use?

Start with first-release cloud coverage; it constrains adapter design,
configuration, and credible allocation policies.

## Engineering constraints to carry forward

These preferences were supplied in the originating workspace. They are useful
implementation guidance; no Weave-specific feature/database architecture is
being imported into this separate project.

- Prefer cohesive, readable control flow and visible side effects. Avoid chains
  of single-use helpers, test-only indirection, and generic utility packages.
- Validate at explicit input and construction boundaries. Make required
  dependencies explicit; avoid repeated defensive nil checks in inner logic.
- Avoid inline/anonymous struct types and `nolint` directives.
- Leave a blank line after a closing code group before the next logical group.
- Use Conventional Commits, imperative subjects of at most 50 characters.
- Use branches such as `feat/<description>` and `fix/<description>`.
- Push with an explicit destination:
  `git push origin HEAD:refs/heads/<branch-name>`.
- Preserve unrelated work. Verify actual branch, base commit, and upstream.
- Choose a model explicitly for delegated work. Use a cheaper capable model for
  bounded research and implementation; explain escalation for hard design or
  diagnosis. Honor current runtime and skill instructions when continuing.
- User-owned skills belong under `~/.agents/skills` when installing or creating
  skills. This project does not require installing any now.

## Implementation risks and acceptance evidence

Build meaningful coverage around the provider contract and observable behavior:

- Every allocated candidate satisfies the profile's hard constraints.
- Strategy decisions are explainable, with source and age of price/capacity
  evidence. Missing signals never masquerade as successful optimization.
- Explicit capacity rejection can select the next permitted candidate within a
  bounded deadline. Auth/configuration errors should remain attributable.
- Ambiguous allocation outcomes are reconciled before attempting a second VM;
  retries must not create duplicates or leave billable resources behind.
- GARM controller/pool identity tags and durable backend identity support
  list/get/delete after restarts and partial failures, including delete by name
  when a create failed before returning a provider ID.
- Listing/reconciliation handles partial cloud outages without falsely treating
  an unreachable cloud's instances as deleted.
- Logical image names resolve to compatible images for the selected cloud,
  region, and architecture. GARM bootstrap metadata remains intact.
- Logs and diagnostics exclude credentials, runner registration material, and
  instance tokens. Stdout remains valid provider protocol output.
- Cloud contract tests and any authorized disposable live run verify cleanup.
  Fixture or mock results alone do not prove cloud provisioning works.

Do not commission a controller or create billable cloud resources solely because
the implementation workflow was invoked. Obtain explicit scope/budget for live
cloud work. Public repository setup was authorized separately.

## Resume the workflow

Use the registered **grill-plan-build** skill and its supporting skills. Reuse
this decision record and research; do not re-ask the project name, owner,
visibility, configurable purchasing, or configurable selection intent.

The intended next sequence is:

1. Inspect this repository and current upstream contracts.
2. Resolve the open product/design decisions through the interview.
3. Verify `.agent/work/` is ignored. It is included in the bootstrap `.gitignore`.
4. Create the work item, decision record, ExecPlan, improvement pass, and task
   graph required by grill-plan-build. No work item exists yet.
5. Present a concrete plan and obtain any remaining approval required by the
   workflow, including explicit Goal activation authorization before activation.
6. Implement, verify, independently review, and produce the authorized handoff.

Suggested opening prompt for the new agent:

```text
Use grill-plan-build to implement Outrider. Read HANDOFF.md and
docs/research/garm-and-runson.md first. Resume the design interview from the open
decisions, starting with first-release cloud coverage. Preserve confirmed user
decisions and distinguish recommendations from approvals.
```
