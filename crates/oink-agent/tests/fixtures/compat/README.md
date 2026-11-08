Frozen v1 execution and retained-register functions, copied byte-for-byte from
0.3.3 build 100 (`3c0f57552`) and in-house 101 (`f403d7dbc`). Both revisions have
identical files. Do not update these snapshots when adding protocol features.

- `layout_100_101.rs` SHA256: `65d596723518bd6b60c4ca1e6ded0360b89af29f089e05240b52698bed626b05`
- `retained_100_101.rs` SHA256: `8b00f998e5b984a6dc932e4a1223acc85b4991081b8fc93a1c21b354ec94fa87`

`c0_compat.rs` sends current encoded C0 Request/Reply/Done through the admission
state machine and frozen execution/digest/editor functions. Additive visibility
must never change the executable geometry keys accepted by these releases.
