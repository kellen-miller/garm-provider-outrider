# Acceptance contracts

Main owns this directory. Workers may read but cannot change fixtures, required
case IDs or expected classifications to match implementation.

These fixtures are synthetic contracts derived from official API documentation,
models and pinned GARM source. They are not recorded cloud responses or evidence
of live account readiness. sources.json records origin, fetch date and digest;
raw source snapshots remain in the ignored work item. cases.json lists required
behavioral test names by target. cargo xtask check-acceptance executes targets and
checks actual outcomes, failing absent, ignored, duplicate or failed cases.

Actual GARM wrapper, full controller and live-cloud evidence are reported
separately. No live-cloud qualification has been performed.
