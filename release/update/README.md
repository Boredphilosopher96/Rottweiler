# Signed update repository inputs

`rw upgrade` fails closed unless release binaries contain a compile-time update
origin and bootstrap root public key. Before the first tag release, maintainers
must perform an offline key ceremony and commit only these public inputs:

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

The tag workflow materializes those seeds as mode-0600 temporary files, signs
the two channel documents, deletes the temporary directory, attests the archive
and metadata bytes, and publishes the artifacts. Signature metadata is not the
host: copy the signed set plus archives to the configured no-redirect update
origin as an explicit release-deployment step.

Manual release signing must pass that same origin explicitly with
`--base-url "$ROTTWEILER_UPDATE_BASE_URL"`; the signer requires every target URL
to equal that base joined with the authenticated archive filename. Stable and
beta documents also share one repository metadata version so channel changes
cannot look like rollback.

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

Beta uses the same shape with `"channel": "beta"`. Stable targets cannot be
prereleases. Metadata versions and expiry times must advance deliberately; the
signer and client reject rollback, wrong-channel, wrong-platform, expired, and
unsigned inputs.
