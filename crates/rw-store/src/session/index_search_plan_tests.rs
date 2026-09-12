#![allow(clippy::expect_used)]
use super::*;

#[test]
fn search_plan_scans_each_posting_list_once_without_correlated_rescans() {
    let root = tempfile::tempdir().expect("root");
    let index = SessionIndex::open(root.path()).expect("index");
    let connection = index.connection().expect("connection");
    for terms in [1, 2, 8] {
        let query = format!("EXPLAIN QUERY PLAN {}", search_sql(terms));
        let mut args = vec![rusqlite::types::Value::Text("\"needle\"".into()); terms];
        args.push(rusqlite::types::Value::Integer(1));
        let mut statement = connection.prepare(&query).expect("query plan");
        let rows = statement
            .query_map(rusqlite::params_from_iter(args), |row| {
                row.get::<_, String>(3)
            })
            .expect("plan rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("plan details");
        assert!(
            !rows.iter().any(|row| row.contains("CORRELATED")),
            "per-result subquery: {rows:?}"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains("sessions_fts") && row.contains("VIRTUAL TABLE"))
                .count(),
            terms,
            "posting scans: {rows:?}"
        );
    }
}

#[test]
fn rejected_candidates_stop_at_the_budget_without_returning_false_truncation() {
    let root = tempfile::tempdir().expect("root");
    let index = SessionIndex::open(root.path()).expect("index");
    let mut connection = index.connection().expect("connection");
    let transaction = connection.transaction().expect("one seed transaction");
    for ordinal in 0..=MAX_SEARCH_CANDIDATES {
        upsert_projection(
            &transaction,
            &SessionProjection {
                summary: SessionSummary {
                    id: format!("candidate-{ordinal}"),
                    title: "needle".into(),
                    updated_unix_ms: 1,
                    cost_micros: 0,
                    turn_count: 0,
                },
                explicit_title: true,
                complete: true,
                source: JournalPrefixIdentity {
                    next_sequence: 1,
                    digest: [0; 32],
                },
                input_claims: vec![1],
            },
        )
        .expect("bounded metadata row");
    }
    transaction.commit().expect("seed publication");
    let mut inspected = 0;
    let result: Result<Vec<(SessionSearchRow, ())>, SessionStoreError> =
        SessionIndex::search_selected_read_only(
            root.path(),
            "needle",
            1,
            &SessionIndexReadControl::new(),
            |_| {
                inspected += 1;
                Ok(None)
            },
        );
    assert!(matches!(
        result,
        Err(SessionStoreError::SearchCandidateLimitExceeded)
    ));
    assert_eq!(inspected, MAX_SEARCH_CANDIDATES);
}
