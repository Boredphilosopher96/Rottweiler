use rusqlite::{Connection, OptionalExtension as _};

use super::BudgetReservationError as Error;
use crate::session::sqlite_schema;

const CALLS: &str = "CREATE TABLE IF NOT EXISTS provider_calls(
    session_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    phase TEXT NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY(session_id,call_id,attempt)
);";
const SUMS: &str = "CREATE TABLE IF NOT EXISTS provider_budget_sums(
    scope TEXT NOT NULL,
    node INTEGER NOT NULL,
    amounts BLOB NOT NULL,
    PRIMARY KEY(scope,node)
);";

pub(super) fn validate(connection: &Connection) -> Result<(), Error> {
    sqlite_schema::validate_table(connection, "provider_calls", CALLS)?;
    sqlite_schema::validate_table(connection, "provider_budget_sums", SUMS)?;
    let calls_exist = table_exists(connection, "provider_calls")?;
    let sums_exist = table_exists(connection, "provider_budget_sums")?;
    if calls_exist != sums_exist {
        return Err(Error::InvalidPlan(
            "incomplete provider accounting authority",
        ));
    }
    if !calls_exist && table_exists(connection, "turn_accounting")? {
        let has_turn_history = connection
            .query_row("SELECT 1 FROM turn_accounting LIMIT 1", [], |_| Ok(()))
            .optional()?
            .is_some();
        if has_turn_history {
            return Err(Error::InvalidPlan(
                "turn-only history has no exact provider receipts; cannot admit new charges",
            ));
        }
    }
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, Error> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(super) fn ensure(connection: &Connection) -> Result<(), Error> {
    // Both tables become visible in one transaction, including first-open races.
    connection.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        validate(connection)?;
        connection.execute_batch(CALLS)?;
        connection.execute_batch(SUMS)?;
        connection.execute_batch("CREATE INDEX IF NOT EXISTS provider_calls_unsettled ON provider_calls(session_id,call_id,attempt) WHERE phase IN ('reserved','started','ambiguous')")?;
        connection.execute_batch("COMMIT")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = connection.execute_batch("ROLLBACK");
    }
    result
}
