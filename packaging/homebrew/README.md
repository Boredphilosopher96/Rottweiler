# Rottweiler Homebrew tap

Install the signed, prebuilt Apple silicon macOS release:

```sh
brew install --cask Boredphilosopher96/tap/rottweiler
```

Upgrade with `brew upgrade --cask rottweiler`.

The pre-v1 macOS archive is checksum-verified but not Apple-notarized. The Cask
removes quarantine after Homebrew verifies the archive's SHA-256 checksum. This
transitional postflight must be removed when Developer ID signing and Apple
notarization are available.

The `Formula/rottweiler.rb` source-build formula remains available for
development, but the Cask is the supported installation path.

HEAD builds resolve Rust, Bun and Zig from repository-owned exact toolchain
contracts. Homebrew verifies the Bun and Zig official release archives; the native
builder verifies the Zig archive again before compiling the patched OpenTUI
source and bundled dependency sources. Its source, patch, flags and compiler
identity are retained in the candidate receipt. Build caches stay beneath the
isolated Cargo target and follow the repository cleanup owner.
