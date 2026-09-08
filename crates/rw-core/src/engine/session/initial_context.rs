//! Immutable admitted prompt sources shared by actor configurations.
use crate::engine::{
    AgentLoopError,
    recovery::{HistoryRead, HistoryWorkingAllowance},
};
use rw_types::{Block, Role, Turn, TurnMeta, allocation::PrepareAllocation};
use std::{fmt, mem::size_of, ops::Deref, sync::Arc};

const MAX_SEGMENTS: usize = 128;
const MAX_TURNS: usize = 4_096;

trait Source: Send + Sync {
    fn turns(&self) -> &[Turn];
}
struct Batch {
    turns: HistoryRead<Vec<Turn>>,
    _allowance: Arc<dyn HistoryWorkingAllowance>,
}
impl Source for Batch {
    fn turns(&self) -> &[Turn] {
        &self.turns
    }
}
struct Single<T> {
    turn: T,
    _allowance: Arc<dyn HistoryWorkingAllowance>,
}
impl<T: Deref<Target = Turn> + Send + Sync> Source for Single<T> {
    fn turns(&self) -> &[Turn] {
        std::slice::from_ref(&self.turn)
    }
}
#[derive(Clone)]
struct Segment {
    source: Arc<dyn Source>,
    start: usize,
    end: usize,
}
impl Segment {
    fn turns(&self) -> &[Turn] {
        &self.source.turns()[self.start..self.end]
    }
}
struct Backing {
    segments: Vec<Segment>,
    // Destroy every reference to source storage before releasing node credit.
    _allowance: Arc<dyn HistoryWorkingAllowance>,
}

/// Cloning an actor shares its admitted source bodies and their resource owners.
/// Mutations allocate a bounded segment table and only copy the changed turn.
#[derive(Clone, Default)]
pub struct InitialSessionContext {
    backing: Option<Arc<Backing>>,
}
impl fmt::Debug for InitialSessionContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(self.iter()).finish()
    }
}
impl InitialSessionContext {
    /// Adopt an aggregate whose producer admitted construction before allocation.
    /// The separate allowance covers this carrier's source and segment metadata.
    /// # Errors
    /// Rejects oversized context or exhausted resident admission.
    pub fn from_owned(
        turns: HistoryRead<Vec<Turn>>,
        mut allowance: Box<dyn HistoryWorkingAllowance>,
    ) -> Result<Self, AgentLoopError> {
        if turns.len() > MAX_TURNS {
            return Err(invalid());
        }
        if turns.is_empty() {
            return Ok(Self::default());
        }
        allowance.resize(node_bytes(1, size_of::<Batch>())?)?;
        let end = turns.len();
        let allowance: Arc<dyn HistoryWorkingAllowance> = Arc::from(allowance);
        Ok(Self {
            backing: Some(Arc::new(Backing {
                segments: vec![Segment {
                    source: Arc::new(Batch {
                        turns,
                        _allowance: Arc::clone(&allowance),
                    }),
                    start: 0,
                    end,
                }],
                _allowance: allowance,
            })),
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &Turn> {
        self.segments()
            .iter()
            .flat_map(|segment| segment.turns().iter())
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.segments()
            .iter()
            .map(|segment| segment.end - segment.start)
            .sum()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.backing.is_none()
    }

    /// Retain an admitted provider result intact; its body is never cloned here.
    /// # Errors
    /// Rejects segment/turn capacity or exhausted resident admission.
    pub fn append_owned<T: Deref<Target = Turn> + Send + Sync + 'static>(
        &mut self,
        turn: T,
        mut allowance: Box<dyn HistoryWorkingAllowance>,
    ) -> Result<(), AgentLoopError> {
        if self.len() >= MAX_TURNS {
            return Err(invalid());
        }
        let count = self.segments().len() + 1;
        allowance.resize(node_bytes(count, size_of::<Single<T>>())?)?;
        let mut segments = Vec::with_capacity(count);
        segments.extend_from_slice(self.segments());
        let allowance: Arc<dyn HistoryWorkingAllowance> = Arc::from(allowance);
        segments.push(Segment {
            source: Arc::new(Single {
                turn,
                _allowance: Arc::clone(&allowance),
            }),
            start: 0,
            end: 1,
        });
        self.backing = Some(Arc::new(Backing {
            segments,
            _allowance: allowance,
        }));
        Ok(())
    }

    /// Share a workspace's admitted sources without copying their bodies.
    /// # Errors
    /// Rejects segment/turn capacity or exhausted resident admission.
    pub(crate) fn append_context(
        &mut self,
        other: &Self,
        mut allowance: Box<dyn HistoryWorkingAllowance>,
    ) -> Result<(), AgentLoopError> {
        if other.is_empty() {
            return Ok(());
        }
        if self.len() + other.len() > MAX_TURNS {
            return Err(invalid());
        }
        let count = self.segments().len() + other.segments().len();
        allowance.resize(node_bytes(count, 0)?)?;
        let mut segments = Vec::with_capacity(count);
        segments.extend_from_slice(self.segments());
        segments.extend_from_slice(other.segments());
        self.replace(segments, allowance);
        Ok(())
    }

    /// Append policy to the first system turn, preserving all untouched sources.
    /// # Errors
    /// Rejects structural limits or exhausted admission before copying content.
    pub(crate) fn append_system_text(
        &mut self,
        text: &str,
        mut allowance: Box<dyn HistoryWorkingAllowance>,
    ) -> Result<(), AgentLoopError> {
        let selected = self
            .segments()
            .iter()
            .enumerate()
            .find_map(|(segment, source)| {
                source
                    .turns()
                    .iter()
                    .position(|turn| turn.role == Role::System)
                    .map(|turn| (segment, turn))
            });
        if selected.is_none() && self.len() >= MAX_TURNS {
            return Err(invalid());
        }
        let source = selected.map(|(segment, turn)| &self.segments()[segment].turns()[turn]);
        let copied = source
            .map_or(Some(size_of::<Turn>()), PrepareAllocation::prepared_bytes)
            .and_then(|bytes| bytes.checked_mul(2))
            .and_then(|bytes| bytes.checked_add(text.len()))
            .and_then(|bytes| bytes.checked_add(4 * size_of::<Block>()))
            .ok_or_else(invalid)?;
        let extra = selected.map_or(1, |(segment, turn)| {
            usize::from(turn > 0) + usize::from(turn + 1 < self.segments()[segment].turns().len())
        });
        let count = self.segments().len() + extra;
        allowance.resize(node_bytes(count, copied)?)?;
        let mut turn = source.cloned().unwrap_or_else(|| Turn {
            role: Role::System,
            blocks: Vec::new(),
            meta: TurnMeta::default(),
        });
        turn.blocks.push(Block::Text {
            text: text.to_owned(),
        });
        // This source owns the body credit separately from the replaceable table.
        let allowance: Arc<dyn HistoryWorkingAllowance> = Arc::from(allowance);
        let owned: Arc<dyn Source> = Arc::new(Single {
            turn: HistoryRead::new(turn, Arc::clone(&allowance)),
            _allowance: Arc::clone(&allowance),
        });
        let replacement = Segment {
            source: owned,
            start: 0,
            end: 1,
        };
        let mut segments = Vec::with_capacity(count);
        if let Some((index, turn)) = selected {
            segments.extend_from_slice(&self.segments()[..index]);
            let previous = &self.segments()[index];
            let split = previous.start + turn;
            if split > previous.start {
                segments.push(Segment {
                    source: Arc::clone(&previous.source),
                    start: previous.start,
                    end: split,
                });
            }
            segments.push(replacement);
            if split + 1 < previous.end {
                segments.push(Segment {
                    source: Arc::clone(&previous.source),
                    start: split + 1,
                    end: previous.end,
                });
            }
            segments.extend_from_slice(&self.segments()[index + 1..]);
        } else {
            segments.push(replacement);
            segments.extend_from_slice(self.segments());
        }
        // The changed source must also retain table credit after subsequent edits.
        self.backing = Some(Arc::new(Backing {
            segments,
            _allowance: allowance,
        }));
        Ok(())
    }

    fn segments(&self) -> &[Segment] {
        self.backing
            .as_ref()
            .map_or(&[], |backing| &backing.segments)
    }
    fn replace(&mut self, segments: Vec<Segment>, allowance: Box<dyn HistoryWorkingAllowance>) {
        self.backing = Some(Arc::new(Backing {
            segments,
            _allowance: Arc::from(allowance),
        }));
    }
}
fn node_bytes(count: usize, source: usize) -> Result<usize, AgentLoopError> {
    if count > MAX_SEGMENTS {
        return Err(invalid());
    }
    count
        .checked_mul(size_of::<Segment>())
        .and_then(|bytes| bytes.checked_add(source))
        .and_then(|bytes| bytes.checked_add(size_of::<Backing>() + 256))
        .ok_or_else(invalid)
}
fn invalid() -> AgentLoopError {
    AgentLoopError::InvalidConfiguration("initial context allocation limit exceeded".into())
}

#[cfg(test)]
mod tests;
