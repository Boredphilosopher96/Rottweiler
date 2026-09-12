//! Fixed-depth time totals. Admission never scans lifetime receipt rows.

use rusqlite::{Connection, OptionalExtension as _, params};

use super::{BudgetCharge, BudgetReservationError as Error};

// YYYYMMDDHHMMSSmmm fits below 2^57, including the largest supported year.
const TIME_END: u64 = 1 << 57;
pub(super) const ROOT_SCOPE: &str = "";
pub(super) const UNKNOWN: usize = 3;
pub(super) const ACTIVE: usize = 4;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Amounts(pub [u128; 5]);

impl Amounts {
    pub(super) fn charge(charge: BudgetCharge) -> Self {
        let mut result = Self::default();
        result.0[unit_index(charge)] = u128::from(charge.amount());
        result
    }

    pub(super) fn add(self, other: Self) -> Result<Self, Error> {
        let mut result = self;
        for (value, delta) in result.0.iter_mut().zip(other.0) {
            *value = value.checked_add(delta).ok_or(Error::Arithmetic)?;
        }
        Ok(result)
    }

    pub(super) fn subtract(self, other: Self) -> Result<Self, Error> {
        let mut result = self;
        for (value, delta) in result.0.iter_mut().zip(other.0) {
            *value = value.checked_sub(delta).ok_or(Error::Arithmetic)?;
        }
        Ok(result)
    }

    fn encode(self) -> [u8; 80] {
        let mut result = [0; 80];
        for (bytes, value) in result.chunks_exact_mut(16).zip(self.0) {
            bytes.copy_from_slice(&value.to_be_bytes());
        }
        result
    }

    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != 80 {
            return Err(Error::InvalidPlan("invalid budget projection row"));
        }
        let mut values = [0; 5];
        for (value, bytes) in values.iter_mut().zip(bytes.chunks_exact(16)) {
            let bytes = <[u8; 16]>::try_from(bytes).map_err(|_| Error::Arithmetic)?;
            *value = u128::from_be_bytes(bytes);
        }
        Ok(Self(values))
    }
}

pub(super) fn unit_index(charge: BudgetCharge) -> usize {
    match charge {
        BudgetCharge::UsdMicros(_) => 0,
        BudgetCharge::AiCreditMicros(_) => 1,
        BudgetCharge::SubscriptionTokens(_) => 2,
    }
}

pub(super) fn time_key(timestamp: &str) -> u64 {
    timestamp
        .bytes()
        .filter(u8::is_ascii_digit)
        .fold(0, |key, digit| key * 10 + u64::from(digit - b'0'))
}

pub(super) fn read(connection: &Connection, scope: &str, node: u64) -> Result<Amounts, Error> {
    let bytes: Option<Vec<u8>> = connection
        .query_row(
            "SELECT CASE WHEN length(amounts)=80 THEN amounts ELSE X'' END FROM provider_budget_sums WHERE scope=?1 AND node=?2",
            params![scope, sql_node(node)?],
            |row| row.get(0),
        )
        .optional()?;
    bytes.map_or(Ok(Amounts::default()), |bytes| Amounts::decode(&bytes))
}

fn update(
    connection: &Connection,
    scope: &str,
    node: u64,
    remove: Amounts,
    add: Amounts,
) -> Result<(), Error> {
    let next = read(connection, scope, node)?.subtract(remove)?.add(add)?;
    if next == Amounts::default() {
        connection.execute(
            "DELETE FROM provider_budget_sums WHERE scope=?1 AND node=?2",
            params![scope, sql_node(node)?],
        )?;
    } else {
        connection.execute(
            "INSERT INTO provider_budget_sums(scope,node,amounts) VALUES(?1,?2,?3) ON CONFLICT(scope,node) DO UPDATE SET amounts=excluded.amounts",
            params![scope, sql_node(node)?, next.encode().as_slice()],
        )?;
    }
    Ok(())
}

pub(super) fn pending(
    connection: &Connection,
    scope: &str,
    remove: Amounts,
    add: Amounts,
) -> Result<(), Error> {
    update(connection, scope, 0, remove, add)
}

pub(super) fn dated(
    connection: &Connection,
    scope: &str,
    timestamp: &str,
    remove: Amounts,
    add: Amounts,
) -> Result<(), Error> {
    let mut node = time_key(timestamp);
    if node == 0 || node >= TIME_END {
        return Err(Error::InvalidPlan(
            "accounting time outside supported range",
        ));
    }
    while node < TIME_END {
        update(connection, scope, node, remove, add)?;
        node += node & node.wrapping_neg();
    }
    Ok(())
}

pub(super) fn through(
    connection: &Connection,
    scope: &str,
    mut node: u64,
) -> Result<Amounts, Error> {
    let mut total = Amounts::default();
    while node > 0 {
        total = total.add(read(connection, scope, node)?)?;
        node &= node - 1;
    }
    Ok(total)
}

fn sql_node(node: u64) -> Result<i64, Error> {
    i64::try_from(node).map_err(|_| Error::Arithmetic)
}
