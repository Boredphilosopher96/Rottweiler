use ed25519_dalek::SigningKey;
use rw_types::update_contract::{SignedEnvelope, signature_message};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use ed25519_dalek::VerifyingKey;
use tempfile::TempDir;

use super::artifacts::hex_digest;
use super::*;

struct SigningFixture {
    root: TempDir,
    rotation: RootRotationArgs,
    root_key: VerifyingKey,
    artifact: Vec<u8>,
}

fn write_private_key(path: &Path, seed: [u8; 32]) -> SigningKey {
    fs::write(path, seed).expect("write private key");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("set private key mode");
    SigningKey::from_bytes(&seed)
}

fn fixture() -> SigningFixture {
    let root = tempfile::tempdir().expect("temporary directory");
    let root_private = root.path().join("root.key");
    let release_private = root.path().join("release.key");
    let root_signer = write_private_key(&root_private, [7; 32]);
    let release_signer = write_private_key(&release_private, [9; 32]);
    let artifact_name = "rottweiler-1.2.3-darwin-arm64.tar.gz";
    let artifact_path = root.path().join(artifact_name);
    let artifact = b"deterministic signed release archive".to_vec();
    fs::write(&artifact_path, &artifact).expect("write artifact");
    let root_spec = root.path().join("root-spec.json");
    fs::write(
        &root_spec,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "role": "root",
            "version": 1,
            "expires_unix": 2_000_000_000_u64,
            "keys": {
                "release-1": STANDARD.encode(release_signer.verifying_key().to_bytes()),
                "root-1": STANDARD.encode(root_signer.verifying_key().to_bytes()),
            },
            "root_key_ids": ["root-1"],
            "root_threshold": 1,
            "release_key_ids": ["release-1"],
            "release_threshold": 1,
        }))
        .expect("serialize root spec"),
    )
    .expect("write root spec");
    let release_spec = |channel: &str, version: u64| {
        json!({
            "schema_version": 1,
            "role": "release",
            "version": version,
            "expires_unix": 1_900_000_000_u64,
            "channel": channel,
            "release_notes": "Signed release notes",
            "targets": {
                "darwin-arm64": {
                    "version": "1.2.3",
                    "url": format!("https://releases.example.invalid/{artifact_name}"),
                }
            }
        })
    };
    let stable_spec = root.path().join("stable-spec.json");
    let beta_spec = root.path().join("beta-spec.json");
    fs::write(
        &stable_spec,
        serde_json::to_vec(&release_spec("stable", 1)).expect("stable spec"),
    )
    .expect("write stable spec");
    fs::write(
        &beta_spec,
        serde_json::to_vec(&release_spec("beta", 1)).expect("beta spec"),
    )
    .expect("write beta spec");
    let rotation = RootRotationArgs {
        root_spec,
        root_chain: None,
        root_keys: vec![("root-1".to_owned(), root_private)],
        output: root.path().join("initial-root"),
    };
    SigningFixture {
        root,
        rotation,
        root_key: root_signer.verifying_key(),
        artifact,
    }
}

fn release_arguments(root: &TempDir, chain: &Path, output_name: &str) -> ReleaseSignArgs {
    ReleaseSignArgs {
        root_chain: chain.to_owned(),
        stable_spec: root.path().join("stable-spec.json"),
        beta_spec: root.path().join("beta-spec.json"),
        base_url: "https://releases.example.invalid/".to_owned(),
        now_unix: 1_800_000_000,
        previous_stable: None,
        previous_beta: None,
        artifacts: vec![root.path().join("rottweiler-1.2.3-darwin-arm64.tar.gz")],
        platforms: vec!["darwin-arm64".to_owned()],
        release_keys: vec![("release-1".to_owned(), root.path().join("release.key"))],
        output: root.path().join(output_name),
    }
}

fn decode_release(path: &Path) -> ReleasePayload {
    let envelope: SignedEnvelope =
        serde_json::from_slice(&fs::read(path).expect("release envelope"))
            .expect("parse release envelope");
    serde_json::from_slice(
        &STANDARD
            .decode(envelope.payload.as_bytes())
            .expect("release payload"),
    )
    .expect("parse release payload")
}

fn set_release_spec_version(path: &Path, version: u64) {
    let mut spec: serde_json::Value =
        serde_json::from_slice(&fs::read(path).expect("release spec")).expect("parse release spec");
    spec["version"] = json!(version);
    fs::write(path, serde_json::to_vec(&spec).expect("release spec bytes"))
        .expect("write release spec");
}

fn sign_argument_reason(result: Result<(), XtaskError>) -> String {
    match result {
        Err(XtaskError::SignArgument(reason)) => reason,
        Err(error) => panic!("expected signing-argument error, got {error}"),
        Ok(()) => panic!("expected signing-argument error"),
    }
}

#[test]
fn signer_emits_the_shared_wire_contract_and_covers_exact_payload_bytes() {
    let fixture = fixture();
    rotate_root(&fixture.rotation).expect("initial root signing");
    let chain = fixture.root.path().join("initial-root/root-chain.json");
    let first = release_arguments(&fixture.root, &chain, "first");
    sign_release(&first).expect("first release signing run");
    let first_output = fixture.root.path().join("first");
    let root_bytes = fs::read(first_output.join("root.json")).expect("root envelope");
    let envelope: SignedEnvelope =
        serde_json::from_slice(&root_bytes).expect("decode root envelope");
    let payload = STANDARD
        .decode(envelope.payload.as_bytes())
        .expect("decode root payload");
    let message = signature_message(UPDATE_ROOT_ROLE, &payload);
    let signature = STANDARD
        .decode(envelope.signatures[0].signature.as_bytes())
        .expect("decode signature");
    fixture
        .root_key
        .verify_strict(
            &message,
            &ed25519_dalek::Signature::from_slice(&signature).expect("signature bytes"),
        )
        .expect("domain-separated signature");

    let stable_bytes = fs::read(first_output.join("stable.json")).expect("stable envelope");
    let stable_envelope: SignedEnvelope =
        serde_json::from_slice(&stable_bytes).expect("decode stable envelope");
    let stable_payload: ReleasePayload = serde_json::from_slice(
        &STANDARD
            .decode(stable_envelope.payload.as_bytes())
            .expect("decode stable payload"),
    )
    .expect("parse stable payload");
    let target = &stable_payload.targets["darwin-arm64"];
    assert_eq!(target.length, fixture.artifact.len() as u64);
    assert_eq!(
        target.sha256,
        hex_digest(&Sha256::digest(&fixture.artifact))
    );
    assert_eq!(
        fs::read_to_string(first_output.join("SHA256SUMS")).expect("checksums"),
        format!("{}  rottweiler-1.2.3-darwin-arm64.tar.gz\n", target.sha256)
    );

    let second_output = fixture.root.path().join("second");
    let second = release_arguments(&fixture.root, &chain, "second");
    sign_release(&second).expect("second release signing run");
    for name in [
        "SHA256SUMS",
        "beta.json",
        "root-chain.json",
        "root.json",
        "stable.json",
    ] {
        assert_eq!(
            fs::read(first_output.join(name)).expect("first output"),
            fs::read(second_output.join(name)).expect("second output")
        );
    }
}

#[test]
fn unsafe_private_key_mode_and_hard_links_fail_before_output() {
    let mode = fixture();
    let key_path = &mode.rotation.root_keys[0].1;
    fs::set_permissions(key_path, fs::Permissions::from_mode(0o644)).expect("weaken key mode");
    assert!(matches!(
        rotate_root(&mode.rotation),
        Err(XtaskError::PrivateKey { .. })
    ));
    assert!(!mode.root.path().join("initial-root").exists());

    let linked = fixture();
    fs::hard_link(
        &linked.rotation.root_keys[0].1,
        linked.root.path().join("root-key-link"),
    )
    .expect("create hard link");
    assert!(matches!(
        rotate_root(&linked.rotation),
        Err(XtaskError::PrivateKey { .. })
    ));
    assert!(!linked.root.path().join("initial-root").exists());
}

#[test]
fn signer_role_mismatch_is_rejected_before_output() {
    let fixture = fixture();
    rotate_root(&fixture.rotation).expect("initial root");
    let chain = fixture.root.path().join("initial-root/root-chain.json");
    let mut release = release_arguments(&fixture.root, &chain, "role-output");
    release.release_keys[0].0 = "root-1".to_owned();
    assert!(matches!(
        sign_release(&release),
        Err(XtaskError::UpdateSpec { .. })
    ));
    assert!(!fixture.root.path().join("role-output").exists());
}

#[test]
fn release_signing_binds_one_shared_version_and_exact_base_url() {
    let missing_slash = fixture();
    rotate_root(&missing_slash.rotation).expect("initial root");
    let chain = missing_slash
        .root
        .path()
        .join("initial-root/root-chain.json");
    let mut arguments = release_arguments(&missing_slash.root, &chain, "missing-slash");
    arguments.base_url = "https://releases.example.invalid/v1".to_owned();
    assert!(matches!(
        sign_release(&arguments),
        Err(XtaskError::SignArgument(_))
    ));

    let divergent = fixture();
    rotate_root(&divergent.rotation).expect("initial root");
    let chain = divergent.root.path().join("initial-root/root-chain.json");
    let beta_path = divergent.root.path().join("beta-spec.json");
    let mut beta: serde_json::Value =
        serde_json::from_slice(&fs::read(&beta_path).expect("beta spec")).expect("beta JSON");
    beta["version"] = json!(5);
    fs::write(&beta_path, serde_json::to_vec(&beta).expect("beta bytes")).expect("write beta");
    let arguments = release_arguments(&divergent.root, &chain, "divergent");
    assert!(matches!(
        sign_release(&arguments),
        Err(XtaskError::SignArgument(_))
    ));

    let wrong_repository = fixture();
    rotate_root(&wrong_repository.rotation).expect("initial root");
    let chain = wrong_repository
        .root
        .path()
        .join("initial-root/root-chain.json");
    let mut arguments = release_arguments(&wrong_repository.root, &chain, "wrong-repository");
    arguments.base_url = "https://releases.example.invalid/v1/".to_owned();
    assert!(matches!(
        sign_release(&arguments),
        Err(XtaskError::UpdateSpec { .. })
    ));
}

#[test]
fn channels_advance_independently_only_from_signed_prior_targets() {
    let fixture = fixture();
    rotate_root(&fixture.rotation).expect("initial root");
    let chain = fixture.root.path().join("initial-root/root-chain.json");
    let initial = release_arguments(&fixture.root, &chain, "initial-release");
    sign_release(&initial).expect("initial release");
    let prior_stable = fixture.root.path().join("initial-release/stable.json");
    let prior_beta = fixture.root.path().join("initial-release/beta.json");

    let beta_name = "rottweiler-1.3.0-beta.1-darwin-arm64.tar.gz";
    let beta_artifact = fixture.root.path().join(beta_name);
    fs::write(&beta_artifact, b"new beta artifact").expect("beta artifact");
    let stable_spec = json!({
        "schema_version": 1,
        "role": "release",
        "version": 2,
        "expires_unix": 1_950_000_000_u64,
        "channel": "stable",
        "release_notes": "Stable remains unchanged",
        "targets": {"darwin-arm64": {
            "version": "1.2.3",
            "url": "https://releases.example.invalid/rottweiler-1.2.3-darwin-arm64.tar.gz"
        }}
    });
    let beta_spec = json!({
        "schema_version": 1,
        "role": "release",
        "version": 2,
        "expires_unix": 1_950_000_000_u64,
        "channel": "beta",
        "release_notes": "New beta",
        "targets": {"darwin-arm64": {
            "version": "1.3.0-beta.1",
            "url": format!("https://releases.example.invalid/{beta_name}")
        }}
    });
    fs::write(
        fixture.root.path().join("stable-spec.json"),
        serde_json::to_vec(&stable_spec).expect("stable spec"),
    )
    .expect("write stable spec");
    fs::write(
        fixture.root.path().join("beta-spec.json"),
        serde_json::to_vec(&beta_spec).expect("beta spec"),
    )
    .expect("write beta spec");
    let mut next = release_arguments(&fixture.root, &chain, "independent");
    next.now_unix = 1_925_000_000;
    next.previous_stable = Some(prior_stable.clone());
    next.previous_beta = Some(prior_beta.clone());
    next.artifacts = vec![beta_artifact];
    sign_release(&next).expect("independent beta release");

    let old = decode_release(&prior_stable);
    let new_stable = decode_release(&fixture.root.path().join("independent/stable.json"));
    let new_beta = decode_release(&fixture.root.path().join("independent/beta.json"));
    assert_eq!(new_stable.version, 2);
    assert_eq!(new_beta.version, 2);
    assert_eq!(
        new_stable.targets["darwin-arm64"],
        old.targets["darwin-arm64"]
    );
    assert_eq!(new_beta.targets["darwin-arm64"].version, "1.3.0-beta.1");

    let mut crossed = release_arguments(&fixture.root, &chain, "crossed");
    crossed.previous_stable = Some(prior_beta);
    crossed.previous_beta = Some(prior_stable);
    crossed.artifacts = vec![fixture.root.path().join(beta_name)];
    assert!(matches!(
        sign_release(&crossed),
        Err(XtaskError::UpdateSpec { .. })
    ));
}

#[test]
fn release_signing_rejects_expired_active_root_and_new_channel_specs() {
    let expired_root = fixture();
    rotate_root(&expired_root.rotation).expect("initial root");
    let chain = expired_root
        .root
        .path()
        .join("initial-root/root-chain.json");
    let mut arguments = release_arguments(&expired_root.root, &chain, "expired-root");
    arguments.now_unix = 0;
    let reason = sign_argument_reason(sign_release(&arguments));
    assert!(reason.contains("positive Unix seconds"));
    arguments.now_unix = 2_000_000_000;
    assert!(matches!(
        sign_release(&arguments),
        Err(XtaskError::UpdateSpec { path, reason })
            if path == chain && reason.contains("active root is expired")
    ));

    let expired_stable = fixture();
    rotate_root(&expired_stable.rotation).expect("initial root");
    let chain = expired_stable
        .root
        .path()
        .join("initial-root/root-chain.json");
    let stable_path = expired_stable.root.path().join("stable-spec.json");
    let mut arguments = release_arguments(&expired_stable.root, &chain, "expired-stable");
    arguments.now_unix = 1_900_000_000;
    assert!(matches!(
        sign_release(&arguments),
        Err(XtaskError::UpdateSpec { path, reason })
            if path == stable_path && reason.contains("expired")
    ));

    let expired_beta = fixture();
    rotate_root(&expired_beta.rotation).expect("initial root");
    let chain = expired_beta
        .root
        .path()
        .join("initial-root/root-chain.json");
    let stable_path = expired_beta.root.path().join("stable-spec.json");
    let beta_path = expired_beta.root.path().join("beta-spec.json");
    let mut stable: serde_json::Value =
        serde_json::from_slice(&fs::read(&stable_path).expect("stable spec")).expect("stable JSON");
    stable["expires_unix"] = json!(1_950_000_000_u64);
    fs::write(
        &stable_path,
        serde_json::to_vec(&stable).expect("stable bytes"),
    )
    .expect("write stable");
    let mut arguments = release_arguments(&expired_beta.root, &chain, "expired-beta");
    arguments.now_unix = 1_900_000_000;
    assert!(matches!(
        sign_release(&arguments),
        Err(XtaskError::UpdateSpec { path, reason })
            if path == beta_path && reason.contains("expired")
    ));
}

#[test]
fn release_metadata_epochs_start_at_one_and_advance_exactly_from_matching_priors() {
    let fixture = fixture();
    rotate_root(&fixture.rotation).expect("initial root");
    let chain = fixture.root.path().join("initial-root/root-chain.json");
    let stable_spec = fixture.root.path().join("stable-spec.json");
    let beta_spec = fixture.root.path().join("beta-spec.json");

    set_release_spec_version(&stable_spec, 2);
    set_release_spec_version(&beta_spec, 2);
    let reason = sign_argument_reason(sign_release(&release_arguments(
        &fixture.root,
        &chain,
        "invalid-initial-epoch",
    )));
    assert!(reason.contains("first channel publication"));

    set_release_spec_version(&stable_spec, 1);
    set_release_spec_version(&beta_spec, 1);
    sign_release(&release_arguments(&fixture.root, &chain, "initial-release"))
        .expect("initial release");
    let prior_stable = fixture.root.path().join("initial-release/stable.json");
    let prior_beta = fixture.root.path().join("initial-release/beta.json");

    set_release_spec_version(&stable_spec, 3);
    set_release_spec_version(&beta_spec, 3);
    let mut skipped = release_arguments(&fixture.root, &chain, "skipped-epoch");
    skipped.previous_stable = Some(prior_stable.clone());
    skipped.previous_beta = Some(prior_beta.clone());
    let reason = sign_argument_reason(sign_release(&skipped));
    assert!(reason.contains("advance exactly"));

    set_release_spec_version(&stable_spec, 2);
    set_release_spec_version(&beta_spec, 2);
    let mut second = release_arguments(&fixture.root, &chain, "second-release");
    second.previous_stable = Some(prior_stable.clone());
    second.previous_beta = Some(prior_beta);
    second.artifacts.clear();
    second.platforms.clear();
    sign_release(&second).expect("second release");

    set_release_spec_version(&stable_spec, 3);
    set_release_spec_version(&beta_spec, 3);
    let mut split = release_arguments(&fixture.root, &chain, "split-prior-epochs");
    split.previous_stable = Some(prior_stable);
    split.previous_beta = Some(fixture.root.path().join("second-release/beta.json"));
    split.artifacts.clear();
    split.platforms.clear();
    let reason = sign_argument_reason(sign_release(&split));
    assert!(reason.contains("prior stable and beta metadata"));
}

#[test]
fn prior_rollback_unsigned_prior_and_unused_artifacts_are_rejected() {
    let fixture = fixture();
    rotate_root(&fixture.rotation).expect("initial root");
    let chain = fixture.root.path().join("initial-root/root-chain.json");
    let initial = release_arguments(&fixture.root, &chain, "initial-release");
    sign_release(&initial).expect("initial release");
    let prior_stable = fixture.root.path().join("initial-release/stable.json");
    let prior_beta = fixture.root.path().join("initial-release/beta.json");

    let unsigned = fixture.root.path().join("unsigned-stable.json");
    let mut envelope: SignedEnvelope =
        serde_json::from_slice(&fs::read(&prior_stable).expect("prior stable"))
            .expect("prior envelope");
    envelope.signatures[0].signature = STANDARD.encode([0_u8; 64]);
    fs::write(
        &unsigned,
        serde_json::to_vec(&envelope).expect("unsigned envelope"),
    )
    .expect("write unsigned prior");
    let mut unsigned_args = release_arguments(&fixture.root, &chain, "unsigned");
    unsigned_args.previous_stable = Some(unsigned);
    unsigned_args.previous_beta = Some(prior_beta.clone());
    assert!(matches!(
        sign_release(&unsigned_args),
        Err(XtaskError::UpdateSpec { .. })
    ));

    let mut stale = release_arguments(&fixture.root, &chain, "stale");
    stale.previous_stable = Some(prior_stable);
    stale.previous_beta = Some(prior_beta);
    assert!(matches!(
        sign_release(&stale),
        Err(XtaskError::SignArgument(_))
    ));

    let downgrade_name = "rottweiler-1.1.0-darwin-arm64.tar.gz";
    let downgrade_path = fixture.root.path().join(downgrade_name);
    fs::write(&downgrade_path, b"older signed artifact").expect("downgrade artifact");
    let channel_spec = |channel: &str, target_version: &str, target_name: &str| {
        json!({
            "schema_version": 1,
            "role": "release",
            "version": 2,
            "expires_unix": 1_950_000_000_u64,
            "channel": channel,
            "release_notes": "rollback fixture",
            "targets": {"darwin-arm64": {
                "version": target_version,
                "url": format!("https://releases.example.invalid/{target_name}")
            }}
        })
    };
    fs::write(
        fixture.root.path().join("stable-spec.json"),
        serde_json::to_vec(&channel_spec("stable", "1.1.0", downgrade_name))
            .expect("downgrade stable"),
    )
    .expect("write downgrade stable");
    fs::write(
        fixture.root.path().join("beta-spec.json"),
        serde_json::to_vec(&channel_spec(
            "beta",
            "1.2.3",
            "rottweiler-1.2.3-darwin-arm64.tar.gz",
        ))
        .expect("carry beta"),
    )
    .expect("write carry beta");
    let mut downgrade = release_arguments(&fixture.root, &chain, "downgrade");
    downgrade.previous_stable = Some(fixture.root.path().join("initial-release/stable.json"));
    downgrade.previous_beta = Some(fixture.root.path().join("initial-release/beta.json"));
    downgrade.artifacts = vec![downgrade_path];
    assert!(matches!(
        sign_release(&downgrade),
        Err(XtaskError::UpdateSpec { .. })
    ));

    let current_name = "rottweiler-1.2.3-darwin-arm64.tar.gz";
    fs::write(
        fixture.root.path().join("stable-spec.json"),
        serde_json::to_vec(&channel_spec("stable", "1.2.3", current_name)).expect("current stable"),
    )
    .expect("write current stable");
    fs::write(
        fixture.root.path().join("beta-spec.json"),
        serde_json::to_vec(&channel_spec("beta", "1.2.3", current_name)).expect("current beta"),
    )
    .expect("write current beta");
    let unused_name = "rottweiler-2.0.0-darwin-arm64.tar.gz";
    let unused_path = fixture.root.path().join(unused_name);
    fs::write(&unused_path, b"unused artifact").expect("unused artifact");
    let mut unused = release_arguments(&fixture.root, &chain, "unused");
    unused.previous_stable = Some(fixture.root.path().join("initial-release/stable.json"));
    unused.previous_beta = Some(fixture.root.path().join("initial-release/beta.json"));
    unused.artifacts.push(unused_path);
    unused.platforms.push("darwin-arm64".to_owned());
    assert!(matches!(
        sign_release(&unused),
        Err(XtaskError::SignArgument(_))
    ));
}

#[test]
fn release_mode_has_no_root_private_key_argument() {
    assert!(matches!(
        SignUpdateCommand::parse(
            ["release", "--root-key", "root-1=/private/root.key"]
                .into_iter()
                .map(str::to_owned)
        ),
        Err(XtaskError::Usage)
    ));
    assert!(matches!(
        SignUpdateCommand::parse(
            [
                "rotate-root",
                "--release-key",
                "release-1=/private/release.key"
            ]
            .into_iter()
            .map(str::to_owned)
        ),
        Err(XtaskError::Usage)
    ));
}

#[test]
fn rotation_appends_only_with_old_and_new_root_thresholds() {
    let fixture = fixture();
    rotate_root(&fixture.rotation).expect("initial signing");
    let root = &fixture.root;
    let prior_chain = root.path().join("initial-root/root-chain.json");
    let new_key_path = root.path().join("new-root.key");
    let new_signer = write_private_key(&new_key_path, [11; 32]);
    let old_public = STANDARD.encode(SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes());
    let release_public =
        STANDARD.encode(SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes());
    fs::write(
        root.path().join("root-spec.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "role": "root",
            "version": 2,
            "expires_unix": 2_100_000_000_u64,
            "keys": {
                "new-root": STANDARD.encode(new_signer.verifying_key().to_bytes()),
                "release-1": release_public,
                "root-1": old_public,
            },
            "root_key_ids": ["new-root"],
            "root_threshold": 1,
            "release_key_ids": ["release-1"],
            "release_threshold": 1,
        }))
        .expect("rotation root spec"),
    )
    .expect("write rotation root spec");

    let rotated = RootRotationArgs {
        root_spec: root.path().join("root-spec.json"),
        root_chain: Some(prior_chain.clone()),
        root_keys: vec![
            ("root-1".to_owned(), root.path().join("root.key")),
            ("new-root".to_owned(), new_key_path.clone()),
        ],
        output: root.path().join("rotated"),
    };
    rotate_root(&rotated).expect("dual-threshold rotation");
    let rotated_chain = root.path().join("rotated/root-chain.json");
    let chain: RootChainDocument = read_spec(&rotated_chain).expect("read rotated root chain");
    assert_eq!(chain.roots.len(), 2);
    let (_, accepted) = load_root_chain(Some(&rotated_chain)).expect("verify rotated chain");
    assert_eq!(accepted.expect("last root").version, 2);

    let conflicting_chain = root.path().join("conflicting-root-chain.json");
    let mut conflicting = chain;
    conflicting.roots.push(conflicting.roots[1].clone());
    fs::write(
        &conflicting_chain,
        serde_json::to_vec(&conflicting).expect("conflicting chain"),
    )
    .expect("write conflicting chain");
    assert!(matches!(
        load_root_chain(Some(&conflicting_chain)),
        Err(XtaskError::UpdateSpec { .. })
    ));

    let missing_old = RootRotationArgs {
        root_spec: root.path().join("root-spec.json"),
        root_chain: Some(prior_chain),
        root_keys: vec![("new-root".to_owned(), new_key_path)],
        output: root.path().join("missing-old"),
    };
    assert!(matches!(
        rotate_root(&missing_old),
        Err(XtaskError::UpdateSpec { .. })
    ));
    assert!(!root.path().join("missing-old").exists());
}
