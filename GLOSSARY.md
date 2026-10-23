# Outrider

Outrider selects VM placements and delegates provisioning to existing GARM providers.

## Language

**Runner profile**:
A named set of VM requirements, purchasing rules, and allocation preferences.
_Avoid_: Runner size, flavor, machine type

**Requirements**:
Hard constraints every selected VM must satisfy, including during fallback.
_Avoid_: Preferences

**Administrator limits**:
The outer limits that repository configuration and job overrides cannot relax.
_Avoid_: Defaults

**Defaults**:
Profile values used when a repository or job supplies no replacement.
_Avoid_: Limits

**Purchasing policy**:
The allowed VM purchasing options and the rules for choosing between them.
_Avoid_: Allocation strategy

**Allocation strategy**:
The rule for choosing among candidates that satisfy requirements and purchasing
policy.
_Avoid_: Purchasing policy

**Candidate**:
A possible VM placement with a cloud, location, machine type, compatible image,
and purchasing option.
_Avoid_: Runner, instance

**Placement**:
The selected candidate associated with a runner's provisioned cloud resources.
_Avoid_: Pool

**Provider**:
An existing GARM provider executable that owns cloud provisioning, guest delivery
and instance lifecycle. Outrider invokes it through GARM's process protocol.
_Avoid_: Outrider's pricing client

**Provider binding**:
A pinned provider executable and administrator-owned configuration for a cloud
account and location. Existing allocations retain their original binding.
_Avoid_: Profile

**Allocation**:
One durable runner reservation, including its selected placement, provider
binding, native instance identity and cleanup state.
_Avoid_: Candidate
