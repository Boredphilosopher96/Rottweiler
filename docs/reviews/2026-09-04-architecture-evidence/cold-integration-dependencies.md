# Cold Rust integration dependencies

Rust source-plugin tests now compile the production plugin host. That host imports SDK exports from `dist`; installing only SDK and TUI dependencies does not create those exports on a cold CI checkout.

The Rust job now runs:

```sh
python3 scripts/ci_inventory.py install plugin-host tui --build-dependencies
```

The inventory resolves actual local `file:` dependencies from package manifests, builds them before installing their consumers, and records installation and build state separately. Explicitly requesting the SDK before the host therefore cannot accidentally skip the SDK build.

Verification: the inventory/cleanup unit suite passed 16 tests, including both cold package request orders. The exact preparation command also passed with pinned Bun 1.3.14. Hosted CI for the new branch has not been claimed from this local result.
