//! Transfer a retained input claim only after its preceding attempt ended.
use super::{
    RecoveryError, RecoveryHead,
    projector::{BatchRows, key},
    state::{BOUNDARIES, Boundary},
};

pub(super) fn claim(
    head: &mut RecoveryHead,
    rows: &BatchRows,
    turn: u64,
) -> Result<(), RecoveryError> {
    for input in &mut head.control.accepted {
        if !input.retained {
            continue;
        }
        let ended: Option<Boundary> = rows.get(key(BOUNDARIES, 0, input.claimed_turn))?;
        if head.control.active.is_some() || turn <= input.claimed_turn || ended.is_none() {
            return Err(RecoveryError::Invalid(
                "retained input claim requires an ended turn",
            ));
        }
        input.claimed_turn = turn;
        input.retained = false;
    }
    Ok(())
}
