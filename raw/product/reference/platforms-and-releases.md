## Supported release targets

| Target | Operating system | Signed archive |
|---|---|---|
| `darwin-arm64` | macOS (arm64) | [Download](https://boredphilosopher96.github.io/Rottweiler/updates/rottweiler-0.1.4-darwin-arm64.tar.gz) |
| `linux-x86_64` | Linux (x86_64) | [Download](https://boredphilosopher96.github.io/Rottweiler/updates/rottweiler-0.1.4-linux-x86_64.tar.gz) |

The release contract understands additional build shapes, but only targets in
the signed update specification are advertised as released platforms.

## Bundle membership

A complete release contains:

- `rw`, the public application and Rust supervisor;
- `rottweiler-tui`, the compiled OpenTUI client;
- the OpenTUI native renderer library;
- `rottweiler-wasm-host`;
- `rottweiler-plugin-host`;
- the archive installer.

The archive contract owns names, modes, per-member size ceilings, and platform
native-library names.

## Updates

`rw upgrade` consumes signed root and channel metadata from the no-redirect
update origin. Stable and beta are explicit channels. Immutable archives remain
available under `/updates/`; the documentation deployment does not own or
modify that subtree.

Browse the [GitHub releases](https://github.com/Boredphilosopher96/Rottweiler/releases)
for archive checksums and attestations.
