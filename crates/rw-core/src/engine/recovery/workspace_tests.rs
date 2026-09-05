#![cfg(test)]
#![allow(clippy::expect_used)]
use super::tests::{append, catch_up};
use super::{CanonicalRecovery, RecoveryError, WorkspaceBootstrap};
use crate::engine::PendingEvent;
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{ClientId, ModeId, WorkspaceRootDescriptor};

fn roots(count: usize) -> Vec<WorkspaceRootDescriptor> {
    (0..count)
        .map(|index| WorkspaceRootDescriptor {
            index: u32::try_from(index).expect("fixture root"),
            path: format!("@root/{index}"),
            machine_local: false,
        })
        .collect()
}

#[test]
fn workspace_bootstrap_reads_only_unapplied_suffix_and_preserves_index_authority() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::SessionCreated {
                driver_client_id: ClientId("driver".into()),
            },
            PendingEvent::WorkspaceRootsChanged {
                generation: 1,
                effective_from_turn: 1,
                roots: roots(2),
            },
        ],
    );
    let bootstrap = WorkspaceBootstrap::read(&journal.read_view()).expect("bootstrap");
    assert_eq!(bootstrap.generation, 1);
    assert_eq!(bootstrap.root_count, 2);
    assert_eq!(bootstrap.scanned_events, 2);
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes).expect("recovery");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let indexed = recovery.head().expect("head");
    drop(recovery);
    let bootstrap = WorkspaceBootstrap::read(&journal.read_view()).expect("cached bootstrap");
    assert_eq!(bootstrap.scanned_events, 0);
    append(
        &mut journal,
        vec![PendingEvent::WorkspaceRootsChanged {
            generation: 2,
            effective_from_turn: 1,
            roots: roots(3),
        }],
    );
    let bootstrap = WorkspaceBootstrap::read(&journal.read_view()).expect("suffix bootstrap");
    assert_eq!(bootstrap.scanned_events, 1);
    assert_eq!(bootstrap.generation, 2);
    assert_eq!(bootstrap.root_count, 3);
    let recovery = CanonicalRecovery::open(&journal.read_view(), &modes).expect("recovery");
    assert_eq!(recovery.head().expect("unchanged index"), indexed);
}

#[test]
fn workspace_discovery_does_not_authorize_an_unknown_mode() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::SessionCreated {
                driver_client_id: ClientId("driver".into()),
            },
            PendingEvent::ModeChanged {
                mode: ModeId("unknown".into()),
                definition_fingerprint: "f".repeat(64),
            },
            PendingEvent::WorkspaceRootsChanged {
                generation: 1,
                effective_from_turn: 1,
                roots: roots(2),
            },
        ],
    );
    assert_eq!(
        WorkspaceBootstrap::read(&journal.read_view())
            .expect("workspace discovery")
            .generation,
        1
    );
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes).expect("recovery");
    assert!(matches!(
        recovery.advance(&journal.read_view(), &modes),
        Err(RecoveryError::Projection(_))
    ));
    assert_eq!(recovery.head().expect("unadvanced").next_sequence, 0);
}

#[test]
fn workspace_bootstrap_rejects_invalid_generation_before_root_discovery() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![PendingEvent::WorkspaceRootsChanged {
            generation: 2,
            effective_from_turn: 1,
            roots: roots(2),
        }],
    );
    assert!(matches!(
        WorkspaceBootstrap::read(&journal.read_view()),
        Err(RecoveryError::Invalid("workspace generation"))
    ));
}
