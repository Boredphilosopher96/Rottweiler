#![expect(clippy::expect_used, reason = "test fixture assertions")]
use super::{MANIFEST_BYTES, decode_fixture, encode_fixture, encode_manifest, scan};
use crate::recording::{
    CapabilityManifest, FIXTURE_VERSION, RecordFixture, RecordedCapabilities, RecordedItem,
    capability_manifest_path, fixture_path, replay_reads, request_hash,
    tests::{request, unique_temp_directory},
};
use crate::{
    CacheBreakpointSupport, Capabilities, FinishReason, ProviderError, ProviderErrorKind,
    ProviderEvent, WireMode,
};
use std::cell::Cell;

fn fixture() -> RecordFixture {
    let request = request();
    RecordFixture {
        version: FIXTURE_VERSION,
        provider: "catalog-test".to_owned(),
        capabilities: RecordedCapabilities::from(&Capabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::NormalizedReplay,
        }),
        model_metadata: None,
        wire_mode: WireMode::NormalizedReplay,
        request_hash: request_hash(&request).expect("hash"),
        occurrence: 0,
        request,
        raw_sse: Vec::new(),
        start_error: None,
        items: vec![RecordedItem::Event {
            event: ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        }],
    }
}

#[test]
fn catalog_streams_all_source_files_and_checks_exact_path_identity() {
    let directory = unique_temp_directory("streaming-catalog");
    std::fs::create_dir_all(&directory).expect("directory");
    let mut fixture = fixture();
    let manifest = CapabilityManifest {
        version: FIXTURE_VERSION,
        provider: fixture.provider.clone(),
        capabilities: fixture.capabilities.clone(),
        model_metadata: None,
    };
    std::fs::write(
        capability_manifest_path(&directory, &fixture.provider),
        encode_manifest(&manifest).expect("manifest"),
    )
    .expect("write manifest");
    for occurrence in 0..1_024 {
        fixture.occurrence = occurrence;
        std::fs::write(
            fixture_path(
                &directory,
                &fixture.provider,
                &fixture.request_hash,
                occurrence,
            ),
            encode_fixture(&fixture).expect("fixture"),
        )
        .expect("write fixture");
    }
    let visited = Cell::new(0);
    let (actual, metadata) = scan(&directory, &fixture.provider, || {
        visited.set(visited.get() + 1);
        Ok(())
    })
    .expect("stream whole catalog");
    assert_eq!(actual, fixture.capabilities);
    assert!(metadata.is_none());
    assert!(visited.get() > 1_024);
    let path = fixture_path(
        &directory,
        &fixture.provider,
        &fixture.request_hash,
        fixture.occurrence,
    );
    fixture.occurrence += 1;
    std::fs::write(path, encode_fixture(&fixture).expect("fixture")).expect("changed identity");
    assert_eq!(
        scan(&directory, &fixture.provider, || Ok(()))
            .expect_err("wrong filename")
            .kind,
        ProviderErrorKind::Protocol
    );
    std::fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn descriptor_admission_rejects_sparse_oversize_before_reading_and_checks_cancellation() {
    let directory = unique_temp_directory("catalog-read-admission");
    std::fs::create_dir_all(&directory).expect("directory");
    let path = directory.join("fixture.json");
    let file = std::fs::File::create(&path).expect("file");
    file.set_len(u64::try_from(replay_reads::MAX_FIXTURE_BYTES + 1).expect("length"))
        .expect("sparse source");
    assert!(replay_reads::read_bounded(&path, replay_reads::MAX_FIXTURE_BYTES, || Ok(())).is_err());
    assert!(replay_reads::read_bounded(&path, MANIFEST_BYTES, || Ok(())).is_err());
    file.set_len(128 * 1024).expect("bounded source");
    let checks = Cell::new(0);
    let error = replay_reads::read_bounded(&path, replay_reads::MAX_FIXTURE_BYTES, || {
        checks.set(checks.get() + 1);
        if checks.get() >= 3 {
            Err(ProviderError::new(
                ProviderErrorKind::Cancelled,
                "cancelled",
            ))
        } else {
            Ok(())
        }
    })
    .expect_err("cancel before second chunk");
    assert_eq!(error.kind, ProviderErrorKind::Cancelled);
    assert_eq!(checks.get(), 3);
    std::fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn structural_admission_rejects_dense_nodes_and_both_encoding_directions_agree() {
    let fixture = fixture();
    let bytes = encode_fixture(&fixture).expect("bounded producer");
    assert_eq!(
        decode_fixture(&bytes).expect("bounded reader").request_hash,
        fixture.request_hash
    );
    let dense = format!(
        "[{}]",
        "null,".repeat(replay_reads::MAX_FIXTURE_BYTES / 128) + "null"
    );
    assert!(dense.len() < replay_reads::MAX_FIXTURE_BYTES);
    assert!(decode_fixture(dense.as_bytes()).is_err());
}
