# Tagged GARM provider wrapper probe

`main.go` is the promoted local process harness. Its module requirements pin
GARM v0.2.1, commit `154638445c3949c1958b01812f69d9a1e4d82684`, and
`garm-provider-common` v0.1.9; `go.sum` records module checksums. The harness
imports GARM's actual v0.1.0 external-provider wrapper.

Build it with `cargo xtask build-garm-wrapper-probe /tmp/garm-wrapper-probe`.
The `tagged_garm_v010_wrapper_process` system test builds and runs it against a
real Outrider daemon process and the concrete AWS HTTP fixture. Set
`OUTRIDER_GARM_WRAPPER_PROBE_OUTPUT` to retain its JSON report at a chosen path.
