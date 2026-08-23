# Signed update repository inputs

`rw upgrade` fails closed unless release binaries contain a compile-time update
origin and bootstrap root public key. Signed releases are published from the
committed public root chain and channel specs. Future root rotations remain an
offline ceremony; only these public inputs belong in the repository:

- `root-chain.json` — exact signed root envelopes, beginning at version 1.
- `stable.spec.json` and `beta.spec.json` — release metadata specs whose target
  URLs point at the no-redirect update origin configured by
  `ROTTWEILER_UPDATE_BASE_URL`.

Create or rotate roots only on an offline Unix host:

```sh
cargo xtask sign-update rotate-root \
  --root-spec release/update/root.1.spec.json \
  --root-key root-1=/private/offline/root.seed \
  --output release/update/rotated
```

For a rotation, pass the existing `--root-chain` plus enough old and new root
keys to satisfy both thresholds. Review the generated public chain, then replace
`root-chain.json`. Root seed files must never enter this repository or GitHub
Actions.

Routine tag releases use only the online release role. Configure these protected
repository values:

- Variables: `ROTTWEILER_UPDATE_ROOT_VERSION`,
  `ROTTWEILER_UPDATE_ROOT_THRESHOLD`, `ROTTWEILER_UPDATE_ROOT_KEYS_JSON`,
  and `ROTTWEILER_UPDATE_BASE_URL`. The root-key value is a JSON object mapping
  every current root-role key id to its canonical base64 32-byte public key; CI
  checks its exact keys and threshold against the latest signed root before
  building. The base URL must be HTTPS and end in `/`
  so relative metadata names stay beneath the intended repository path.
- Secret: `ROTTWEILER_UPDATE_RELEASE_KEYS_JSON`, a JSON object mapping every
  release-role key id required by the current threshold to the canonical
  base64 encoding of its exact 32-byte Ed25519 seed. Key ids and seed material
  must both be unique; at most 32 keys are accepted.

For v1 and later, the protected `release` environment also supplies the paid
live-smoke keys, a dated OpenAI or Anthropic evaluation model, its reviewed
Terminal-Bench baseline, and the external dogfood-ledger secret documented in
`docs/07-VERIFICATION.md`. Those v1 qualification inputs are not required for a
pre-v1 tag. GitHub Models was retired on 2026-07-30 and is not a release
dependency; Terminal-Bench selects the matching paid provider key only inside
its step. For v1 and later, the native macOS ARM64 runner and the Linux X64
soak runner must be online. Linux core measurements, WSL2,
and Harbor's containers use fixed disposable GitHub-hosted images. These are
prerequisites to signing for the applicable release tier: the workflow does not
offer a skip flag for missing evidence or infrastructure.

Before creating a tag, run the **Release preflight** workflow manually. It
validates the measured baseline provenance, committed public signing inputs,
protected variables/secrets, and dogfood ledger, then invokes the same
calibrated protected-performance workflow used by release qualification. Its
final artifact binds the readiness and both platform performance evidence sets
to the exact source SHA, version, run, and run attempt. The tag publisher
requires and verifies that artifact; it does not repeat the noisy measurements.
The preflight cannot sign metadata, publish a GitHub release, update Homebrew, or
substitute for the exact-tag WSL2 gate. For v1 and later it also cannot
substitute for the exact-tag protected soaks, Terminal-Bench, or live replay
gates. Pre-v1 readiness records those as not claimed rather than measured.
Before sealing the candidate, preflight also downloads the public stable and
beta envelopes and requires the checked-in channel specs to advance their
shared metadata version by exactly one. Cryptographic authentication and the
complete transition policy remain enforced again by the signer.

The tag workflow materializes those seeds as mode-0600 temporary files, signs
the two channel documents, deletes the temporary directory, attests the archive
and metadata bytes, and publishes the artifacts. It then overlays the signed
set and archives onto the persistent `gh-pages` update repository. Historical
archives are retained, and the exact repository commit is verified after push.

Manual release signing must pass that same origin explicitly with
`--base-url "$ROTTWEILER_UPDATE_BASE_URL"` and a single captured signing time as
`--now-unix "$RELEASE_NOW_UNIX"`; the signer requires every target URL to equal
that base joined with the authenticated archive filename. It rejects an active
root or either new channel document whose expiry is not later than that fixed
time. Stable and beta documents also share one repository metadata version so
channel changes cannot look like rollback.

Stable and beta targets remain independent. Before routine signing, the release
workflow downloads the previously deployed `stable.json` and `beta.json`
without redirects and passes them as `--previous-stable` / `--previous-beta`.
When a spec target has no matching current archive (for example, a beta
prerelease while stable remains on the prior production build), the signer
carries it forward only if its exact version and URL occur in the corresponding
prior envelope and that envelope meets the active release-role threshold.
Cross-channel envelopes, unsigned hashes, target downgrades, mismatched or
unused archives, and invalid metadata transitions are rejected. The first
publication omits both prior-envelope flags, uses metadata version 1, and must
provide matching artifacts for every target in both channel specs. Every later
publication requires both prior envelopes at the same metadata version and
advances the shared version exactly from `N` to `N+1`. Prior channel envelopes
are authenticated historical transition inputs, so their expiry may precede
the fixed signing time; only the active root and newly emitted documents must
still be live.

The channel specs use this shape; the signer fills authenticated length and
SHA-256 values from the exact archives:

```json
{
  "schema_version": 1,
  "role": "release",
  "version": 1,
  "expires_unix": 2000000000,
  "channel": "stable",
  "release_notes": "Release notes",
  "targets": {
    "darwin-arm64": {
      "version": "1.0.0",
      "url": "https://updates.example.invalid/v1/rottweiler-1.0.0-darwin-arm64.tar.gz"
    },
    "linux-x86_64": {
      "version": "1.0.0",
      "url": "https://updates.example.invalid/v1/rottweiler-1.0.0-linux-x86_64.tar.gz"
    }
  }
}
```

Beta uses the same shape with `"channel": "beta"` and may name a different
semantic target version/archive. Stable targets cannot be prereleases. The two
top-level metadata `version` values must remain equal for every publication,
while target versions may differ. Publications advance the shared metadata
version exactly once; a client may then accept that same authenticated version
when switching channels. Expiry times must advance deliberately; the signer and
client reject rollback, wrong-channel, wrong-platform, expired, and unsigned
inputs.
