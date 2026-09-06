//! Release workload for the actual runtime-owned workspace index registry.
use super::*;
use std::time::Instant;

mod source;
use source::{DEFINITIONS, FILE_BYTES, Repository};
type TestResult = Result<(), Box<dyn std::error::Error>>;
const SESSIONS: usize = 8;
const COLD_SAMPLES: usize = 3;
const WARM_SAMPLES: usize = 20;
const INCREMENTAL_SAMPLES: usize = 10;
const BRANCH_SAMPLES: usize = 3;

#[test]
#[ignore = "release acceptance: 100/1k/10k real source files; external process RSS measurement"]
fn qualify_shared_repository_index() -> TestResult {
    assert!(
        !cfg!(debug_assertions),
        "precompile this acceptance in release mode"
    );
    let selected = std::env::var("ROTTWEILER_INTELLIGENCE_ACCEPTANCE_FILES")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?;
    if selected.is_some_and(|files| ![100, 1_000, 10_000].contains(&files)) {
        return Err("acceptance size must be 100, 1000 or 10000".into());
    }
    for files in [100, 1_000, 10_000]
        .into_iter()
        .filter(|files| selected.is_none_or(|selected| selected == *files))
    {
        // Filesystem setup is outside every measured interval and keeps one 4KiB
        // source buffer, never an in-memory repository corpus.
        let repository = Repository::seed(files)?;
        println!(
            "{}",
            json!({"schema_version":1,"workload":"shared_repository_index",
            "phase":"source","files":files,"source_bytes":files*FILE_BYTES,
            "source_digest":repository.digest,"max_file_bytes":FILE_BYTES,"definitions":files*DEFINITIONS,
            "sessions":SESSIONS,"cold_samples":COLD_SAMPLES,"warm_samples":WARM_SAMPLES,
            "incremental_samples":INCREMENTAL_SAMPLES,"branch_pairs":BRANCH_SAMPLES,
            "batch_replacement_files":files/10,"external_edit_add_delete_files_per_sample":[1,1,1],
            "fixture_source_buffer_bytes":FILE_BYTES,"query_limit":1,
            "parser_concurrency":1,"production_max_source_bytes":IndexLimits::default().max_file_bytes,
            "retained_limit":IndexLimits::default().max_retained_bytes,
            "physical_read_counters":null,"parser_heap_bytes":null,
            "rss_scope":"external process maximum RSS includes parser, index, allocator and fixture; not separate parser bytes",
            "cold_scope":"fresh registry in one process; filesystem page cache is not evicted"})
        );
        for sample in 0..COLD_SAMPLES {
            cold_and_warm(&repository, sample)?;
        }
        incremental(&repository)?;
    }
    Ok(())
}

fn sessions(
    pool: &WorkspaceIndexPool,
    repository: &Repository,
) -> Result<Vec<WorkspaceSymbolIndex>, IntelError> {
    let roots = [repository.root.path().to_path_buf()];
    (0..SESSIONS)
        .map(|_| pool.workspace(&roots, &[true]))
        .collect()
}

fn emit(
    phase: &str,
    repository: &Repository,
    sample: usize,
    elapsed: std::time::Duration,
    pool: &WorkspaceIndexPool,
    generation: u64,
) {
    let retained = pool.budget.retained_bytes();
    assert!(retained <= IndexLimits::default().max_retained_bytes);
    println!(
        "{}",
        json!({"schema_version":1,"workload":"shared_repository_index",
        "phase":phase,"files":repository.files,"sample":sample,"elapsed_ns":elapsed.as_nanos(),
        "retained_bytes":retained,"generation":generation})
    );
}

fn cold_and_warm(repository: &Repository, sample: usize) -> TestResult {
    let pool = WorkspaceIndexPool::default();
    let sessions = sessions(&pool, repository)?;
    for session in &sessions[1..] {
        assert!(Arc::ptr_eq(&sessions[0].indexes[0], &session.indexes[0]));
    }
    let started = Instant::now();
    sessions[0].ensure_current()?;
    emit(
        "initial_index",
        repository,
        sample,
        started.elapsed(),
        &pool,
        sessions[0].indexes[0].generation(),
    );
    assert!(!sessions[0].indexes[0].is_partial());
    let generation = sessions[0].indexes[0].generation();
    let retained = pool.budget.retained_bytes();
    let oracle = repository.verify_all(&sessions[0], 0)?;
    println!(
        "{}",
        json!({"schema_version":1,"phase":"oracle","files":repository.files,
        "sample":sample,"ordered_definition_digest":oracle})
    );
    for warm in 0..WARM_SAMPLES {
        let session = &sessions[warm % SESSIONS];
        let started = Instant::now();
        session.ensure_current()?;
        emit(
            "shared_readiness",
            repository,
            sample * WARM_SAMPLES + warm,
            started.elapsed(),
            &pool,
            generation,
        );
        assert_eq!(session.indexes[0].generation(), generation);
        assert_eq!(pool.budget.retained_bytes(), retained);
        let file = (warm * 997) % repository.files;
        let query = SymbolQuery {
            pattern: Repository::name(file, 'a', 0),
            roles: vec![SymbolRole::Definition],
            languages: Vec::new(),
            limit: 1,
        };
        let started = Instant::now();
        let hits = session.query(&query)?;
        emit(
            "exact_symbol_query",
            repository,
            sample * WARM_SAMPLES + warm,
            started.elapsed(),
            &pool,
            generation,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, query.pattern);
        assert_eq!(hits[0].location.path, Repository::path(file));
    }
    // A forced reconciliation exercises unchanged descriptor stamps even when the
    // normal two-second freshness window would skip the scan.
    let started = Instant::now();
    sessions[1].index_workspaces()?;
    emit(
        "unchanged_reconciliation",
        repository,
        sample,
        started.elapsed(),
        &pool,
        generation,
    );
    assert_eq!(sessions[0].indexes[0].generation(), generation);
    assert_eq!(pool.budget.retained_bytes(), retained);
    verify_partitioning(&pool, repository, &sessions[0])?;
    drop(sessions);
    assert_eq!(pool.budget.retained_bytes(), 0);
    Ok(())
}

fn verify_partitioning(
    pool: &WorkspaceIndexPool,
    repository: &Repository,
    shared: &WorkspaceSymbolIndex,
) -> TestResult {
    let untrusted = pool.workspace(&[repository.root.path().to_path_buf()], &[false])?;
    let worktree = tempfile::tempdir()?;
    let separate = pool.workspace(&[worktree.path().to_path_buf()], &[true])?;
    assert!(!Arc::ptr_eq(&shared.indexes[0], &untrusted.indexes[0]));
    assert!(!Arc::ptr_eq(&shared.indexes[0], &separate.indexes[0]));
    assert!(untrusted.symbols_for_file(Repository::path(0))?.is_empty());
    assert!(separate.symbols_for_file(Repository::path(0))?.is_empty());
    Ok(())
}

fn incremental(repository: &Repository) -> TestResult {
    let pool = WorkspaceIndexPool::default();
    let sessions = sessions(&pool, repository)?;
    sessions[0].ensure_current()?;
    for sample in 0..INCREMENTAL_SAMPLES {
        repository.write(sample, 'b')?;
        repository.write(repository.files + sample, 'b')?;
        repository.remove(repository.files - 1 - sample)?;
        let before = sessions[0].indexes[0].generation();
        let started = Instant::now();
        sessions[sample % SESSIONS].index_workspaces()?;
        emit(
            "external_edit_add_delete",
            repository,
            sample,
            started.elapsed(),
            &pool,
            sessions[0].indexes[0].generation(),
        );
        assert!(sessions[0].indexes[0].generation() > before);
        repository.verify_file(&sessions[(sample + 1) % SESSIONS], sample, Some('b'))?;
        repository.verify_file(&sessions[0], repository.files + sample, Some('b'))?;
        repository.verify_file(&sessions[0], repository.files - 1 - sample, None)?;
        assert!(!sessions[0].indexes[0].is_partial());
    }
    for sample in 0..INCREMENTAL_SAMPLES {
        repository.write(sample, 'a')?;
        repository.remove(repository.files + sample)?;
        repository.write(repository.files - 1 - sample, 'a')?;
    }
    sessions[0].index_workspaces()?;
    let baseline = repository.verify_all(&sessions[0], 0)?;
    branch_replacements(repository, &pool, &sessions, &baseline)?;
    drop(sessions);
    assert_eq!(pool.budget.retained_bytes(), 0);
    Ok(())
}

fn branch_replacements(
    repository: &Repository,
    pool: &WorkspaceIndexPool,
    sessions: &[WorkspaceSymbolIndex],
    baseline: &str,
) -> TestResult {
    let changed = repository.files / 10;
    for sample in 0..BRANCH_SAMPLES {
        for file in 0..changed {
            repository.write(file, 'b')?;
        }
        let before = sessions[0].indexes[0].generation();
        let started = Instant::now();
        sessions[0].index_workspaces()?;
        emit(
            "branch_batch_replacement",
            repository,
            sample,
            started.elapsed(),
            pool,
            sessions[0].indexes[0].generation(),
        );
        assert!(sessions[0].indexes[0].generation() > before);
        assert_ne!(repository.verify_all(&sessions[1], changed)?, baseline);
        for file in 0..changed {
            repository.write(file, 'a')?;
        }
        let started = Instant::now();
        sessions[2].index_workspaces()?;
        emit(
            "branch_batch_restore",
            repository,
            sample,
            started.elapsed(),
            pool,
            sessions[0].indexes[0].generation(),
        );
        assert_eq!(repository.verify_all(&sessions[3], 0)?, baseline);
        assert!(!sessions[0].indexes[0].is_partial());
    }
    Ok(())
}

#[test]
fn shared_repository_workload_oracle_covers_external_replacements() -> TestResult {
    let repository = Repository::seed(100)?;
    cold_and_warm(&repository, 0)?;
    incremental(&repository)
}
