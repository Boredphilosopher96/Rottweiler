use ed25519_dalek::{Signer as _, SigningKey};
use flate2::{Compression, write::GzEncoder};
use rw_types::update_contract::signature_message;
use serde_json::json;
use tar::{Builder, Header};

use super::*;

fn sign_envelope(role: &str, payload: &serde_json::Value, keys: &[(&str, &SigningKey)]) -> Vec<u8> {
    let payload = serde_json::to_vec(payload).expect("payload");
    let message = signature_message(role, &payload);
    serde_json::to_vec(&json!({
        "payload": STANDARD.encode(&payload),
        "signatures": keys.iter().map(|(id, key)| json!({
            "key_id": id,
            "signature": STANDARD.encode(key.sign(&message).to_bytes()),
        })).collect::<Vec<_>>(),
    }))
    .expect("envelope")
}

fn signed_root_entry(
    version: u64,
    expires: u64,
    root_id: &str,
    root_key: &SigningKey,
    release_id: &str,
    release_key: &SigningKey,
    signers: &[(&str, &SigningKey)],
) -> RootChainEntry {
    let envelope = sign_envelope(
        "root",
        &json!({
            "schema_version": 1,
            "role": "root",
            "version": version,
            "expires_unix": expires,
            "keys": {
                (root_id): STANDARD.encode(root_key.verifying_key().to_bytes()),
                (release_id): STANDARD.encode(release_key.verifying_key().to_bytes()),
            },
            "root_key_ids": [root_id],
            "root_threshold": 1,
            "release_key_ids": [release_id],
            "release_threshold": 1,
        }),
        signers,
    );
    RootChainEntry {
        version,
        envelope: STANDARD.encode(envelope),
    }
}

fn archive_fixture(link_rw: bool, unexpected: bool) -> Vec<u8> {
    let root = release_archive_root("1.2.3", release_platform());
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = Builder::new(encoder);
    for directory in [&root, &format!("{root}/bin")] {
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_data(&mut header, directory, std::io::empty())
            .expect("directory fixture");
    }
    let platform = platform_for_rust_target(std::env::consts::OS, std::env::consts::ARCH)
        .expect("supported test platform");
    for member in platform.archive_members {
        let mut header = Header::new_gnu();
        let is_engine = member.id == "engine";
        if is_engine && link_rw {
            header.set_entry_type(EntryType::Symlink);
            header.set_link_name("../../outside").expect("link target");
            header.set_size(0);
        } else {
            header.set_entry_type(EntryType::Regular);
            header.set_size(member.id.len() as u64);
        }
        header.set_mode(member.mode);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("{root}/{}", member.path),
                if is_engine && link_rw {
                    b"".as_slice()
                } else {
                    member.id.as_bytes()
                },
            )
            .expect("release-contract member fixture");
    }
    if unexpected {
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(4);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("{root}/extra"), b"evil".as_slice())
            .expect("unexpected fixture");
    }
    let encoder = builder.into_inner().expect("tar fixture");
    encoder.finish().expect("gzip fixture")
}

#[test]
fn drvfs_mount_detection_is_longest_prefix_and_wsl_specific() {
    let mounts = "none /mnt/c 9p rw,dirsync,aname=drvfs 0 0\n/dev/sda / ext4 rw 0 0\n";
    assert!(path_is_drvfs(Path::new("/mnt/c/Users/test/rw"), mounts));
    assert!(!path_is_drvfs(Path::new("/home/test/rw"), mounts));
    assert!(looks_like_wsl_drive_path(Path::new("/mnt/d/tools")));
    assert!(!looks_like_wsl_drive_path(Path::new("/home/tools")));
}

#[test]
fn unknown_or_package_managed_layout_is_refused() {
    let root = tempfile::tempdir().expect("root");
    let direct = root.path().join("rw");
    fs::write(&direct, b"binary").expect("fixture");
    assert!(InstallLayout::from_executable(&direct).is_err());
}

#[test]
fn unsupported_homebrew_layout_preserves_refusal_with_package_guidance() {
    let homebrew = unsupported_layout_for(Some(Path::new(
        "/opt/homebrew/Caskroom/rottweiler/0.1.4/rottweiler-0.1.4-darwin-arm64/bin/rw",
    )))
    .to_string();
    assert!(homebrew.contains("brew upgrade rottweiler"));
    assert!(homebrew.contains("never modifies package-managed files"));

    let formula = unsupported_layout_for(Some(Path::new(
        "/home/linuxbrew/.linuxbrew/Cellar/rottweiler/0.1.4/libexec/rw",
    )))
    .to_string();
    assert!(formula.contains("brew upgrade rottweiler"));

    let unknown = unsupported_layout_for(Some(Path::new("/usr/local/bin/rw"))).to_string();
    assert!(unknown.contains("official versioned installation layout"));
    assert!(!unknown.contains("brew upgrade"));
}

#[test]
fn embedded_update_base_url_requires_repository_trailing_slash() {
    assert!(validate_embedded_update_base_url("https://updates.example/v1/").is_ok());
    assert!(validate_embedded_update_base_url("https://updates.example/v1").is_err());
    assert!(validate_embedded_update_base_url("https://updates.example/v1/?x=1").is_err());
}

#[test]
fn upgrade_lock_refuses_live_owner_and_recovers_dead_owner() {
    let live_root = tempfile::tempdir().expect("live root");
    let live = UpgradeLock::acquire(live_root.path()).expect("live lock");
    assert!(UpgradeLock::acquire(live_root.path()).is_err());
    drop(live);

    let stale_root = tempfile::tempdir().expect("stale root");
    let stale = stale_root.path().join(".install-lock");
    fs::create_dir(&stale).expect("stale lock");
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o700)).expect("stale mode");
    let owner = stale.join("pid");
    fs::write(&owner, b"2147483647\n").expect("stale owner");
    fs::set_permissions(&owner, fs::Permissions::from_mode(0o600)).expect("owner mode");
    let recovered = UpgradeLock::acquire(stale_root.path()).expect("recover stale lock");
    assert_eq!(
        fs::read_to_string(stale.join("pid"))
            .expect("new owner")
            .trim(),
        std::process::id().to_string()
    );
    drop(recovered);
}

#[test]
fn managed_layout_requires_selected_generation_and_launcher() {
    let root = tempfile::tempdir().expect("root");
    let generation = root.path().join("versions/1.2.3/bin");
    fs::create_dir_all(&generation).expect("generation");
    let executable = generation.join("rw");
    fs::write(&executable, b"binary").expect("fixture");
    fs::create_dir(root.path().join("bin")).expect("bin");
    symlink("versions/1.2.3", root.path().join("current")).expect("current");
    symlink("../current/bin/rw", root.path().join("bin/rw")).expect("launcher");
    let layout = InstallLayout::from_executable(&executable).expect("managed layout");
    assert_eq!(layout.current_version, "1.2.3");
}

#[test]
fn update_state_rejects_control_characters_and_unsafe_selectors() {
    let generation = Generation {
        version: "1.2.3".to_owned(),
        platform: release_platform().to_owned(),
        files: BTreeMap::new(),
    };
    let mut state = UpgradeState {
        schema_version: 1,
        highest_root_version: 1,
        highest_metadata_version: 1,
        trusted_unix_time: 1,
        trusted_root_chain: Vec::new(),
        active: generation,
        previous: None,
        pending_release_notes: Some(PendingReleaseNotes {
            version: "1.2.3".to_owned(),
            notes: "bad\u{0007}".to_owned(),
        }),
    };
    assert!(validate_state(&state).is_err());
    state.pending_release_notes = None;
    state.active.version = "../escape".to_owned();
    assert!(validate_state(&state).is_err());
}

#[test]
fn initialized_state_marker_makes_state_deletion_fail_closed() {
    let root = tempfile::tempdir().expect("root");
    fs::write(root.path().join(STATE_MARKER), b"1\n").expect("marker");
    let layout = InstallLayout {
        prefix: root.path().to_path_buf(),
        versions: root.path().join("versions"),
        current_version: "1.2.3".to_owned(),
    };
    assert!(load_or_bootstrap_state(&layout).is_err());
}

#[test]
fn state_active_version_must_match_current_selector() {
    let root = tempfile::tempdir().expect("root");
    let layout = InstallLayout {
        prefix: root.path().to_path_buf(),
        versions: root.path().join("versions"),
        current_version: "1.2.3".to_owned(),
    };
    let state = UpgradeState {
        schema_version: 1,
        highest_root_version: 0,
        highest_metadata_version: 0,
        trusted_unix_time: 0,
        trusted_root_chain: Vec::new(),
        active: Generation {
            version: "9.9.9".to_owned(),
            platform: release_platform().to_owned(),
            files: BTreeMap::new(),
        },
        previous: None,
        pending_release_notes: None,
    };
    assert!(validate_layout_state(&layout, &state).is_err());
}

#[test]
fn exact_archive_allowlist_extracts_without_links_or_extra_entries() {
    let root = tempfile::tempdir().expect("root");
    let staging = root.path().join("staging");
    fs::create_dir(&staging).expect("staging");
    extract_exact_archive(
        &staging,
        "1.2.3",
        release_platform(),
        &archive_fixture(false, false),
    )
    .expect("exact archive");
    assert_eq!(fs::read(staging.join("bin/rw")).expect("rw"), b"engine");
    assert_eq!(
        fs::read(staging.join("bin/rottweiler-wasm-host")).expect("WASM host"),
        b"wasm_host"
    );
    assert!(
        fs::symlink_metadata(staging.join("bin/rw"))
            .expect("metadata")
            .is_file()
    );
}

#[test]
fn archive_links_and_unexpected_entries_fail_closed_in_staging() {
    for artifact in [archive_fixture(true, false), archive_fixture(false, true)] {
        let root = tempfile::tempdir().expect("root");
        let staging = root.path().join("staging");
        fs::create_dir(&staging).expect("staging");
        assert!(extract_exact_archive(&staging, "1.2.3", release_platform(), &artifact).is_err());
        assert!(!root.path().join("outside").exists());
    }
}

#[test]
fn persisted_v3_trust_accepts_v4_after_historical_v2_expiry() {
    let first = SigningKey::from_bytes(&[31; 32]);
    let second = SigningKey::from_bytes(&[32; 32]);
    let third = SigningKey::from_bytes(&[33; 32]);
    let fourth = SigningKey::from_bytes(&[34; 32]);
    let second_release = SigningKey::from_bytes(&[35; 32]);
    let third_release = SigningKey::from_bytes(&[36; 32]);
    let fourth_release = SigningKey::from_bytes(&[37; 32]);
    let embedded = TrustedRoot::from_keys(
        1,
        1,
        [("root-1".to_owned(), first.verifying_key().to_bytes())],
    )
    .expect("embedded root");
    let v2 = signed_root_entry(
        2,
        101,
        "root-2",
        &second,
        "release-2",
        &second_release,
        &[("root-1", &first), ("root-2", &second)],
    );
    let v3 = signed_root_entry(
        3,
        1_000,
        "root-3",
        &third,
        "release-3",
        &third_release,
        &[("root-2", &second), ("root-3", &third)],
    );
    let v4 = signed_root_entry(
        4,
        1_000,
        "root-4",
        &fourth,
        "release-4",
        &fourth_release,
        &[("root-3", &third), ("root-4", &fourth)],
    );
    let selection = restore_and_select_roots(&embedded, 3, &[v2, v3], vec![v4])
        .expect("restore v3 and select v4");
    assert_eq!(selection.trusted.version(), 3);
    let release = sign_envelope(
        "release",
        &json!({
            "schema_version": 1,
            "role": "release",
            "version": 7,
            "expires_unix": 900,
            "channel": "stable",
            "release_notes": "v4",
            "targets": {
                (release_platform()): {
                    "version": "1.1.0",
                    "url": "https://release.example.invalid/v4.tar.gz",
                    "length": 1,
                    "sha256": "00".repeat(32),
                }
            }
        }),
        &[("release-4", &fourth_release)],
    );
    let successor_slices = selection
        .successor_envelopes
        .iter()
        .map(Vec::as_slice)
        .collect::<Vec<_>>();
    assert!(
        verify_update_metadata_chain(
            &selection.trusted,
            &successor_slices,
            &release,
            &UpdateVerificationPolicy {
                channel: UpdateChannel::Stable,
                platform: release_platform(),
                current_version: "1.0.0",
                now_unix: 200,
                high_water: UpdateHighWaterMark {
                    root_version: 3,
                    metadata_version: 6,
                    trusted_unix_time: 100,
                },
                allow_downgrade: false,
            },
        )
        .is_ok()
    );
}
