# Outrider

Configurable multi-cloud runner placement for GARM.

Outrider will resolve named runner profiles into virtual machines using
configurable requirements, purchasing options, and allocation strategies.
Spot and on-demand instances are both in scope. Cost is one possible selection
criterion.

This repository contains the design handoff. No provider implementation exists yet.

- [Project handoff](HANDOFF.md): decisions, open questions, proposed architecture,
  and instructions for the next agent.
- [Upstream research](docs/research/garm-and-runson.md): GARM integration,
  provisioning behavior, provider support, and the RunsOn reference.

## Intended flow

```text
GitHub job requests a named runner profile
  -> GARM requests a runner from Outrider
  -> Outrider resolves requirements and allocation policy
  -> a cloud adapter provisions an eligible VM
  -> the runner registers with GitHub and executes its job
  -> GARM requests cleanup through Outrider
```

Configuration syntax, supported clouds in the first release, implementation
language and strategy semantics remain to be decided.

## License

Outrider is source-available under the PolyForm Perimeter License 1.0.0.
Use, modification, and redistribution are permitted subject to its terms;
providing a competing product, including one offered free of charge, is
prohibited. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
