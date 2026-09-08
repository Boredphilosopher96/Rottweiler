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
