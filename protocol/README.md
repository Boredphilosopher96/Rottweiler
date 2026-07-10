# Generated protocol contract

Rust types in `crates/rw-types` are the only source of truth. Do not edit the
schemas or `types.ts` by hand.

```console
cargo xtask codegen
cargo xtask codegen --check
```

The first command refreshes committed artifacts. The second exits non-zero when
the generated output differs, and is the CI drift gate. Protocol structs allow
unknown object fields so additive schema evolution remains backward compatible.
