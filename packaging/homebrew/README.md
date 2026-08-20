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
