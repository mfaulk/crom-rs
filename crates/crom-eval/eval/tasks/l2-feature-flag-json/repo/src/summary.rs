use crate::model::Event;

/// Aggregate statistics over a set of events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub count: u64,
    pub total_ms: u64,
    pub max_ms: u64,
}

impl Summary {
    pub fn from_events(events: &[Event]) -> Self {
        events.iter().fold(Self::default(), |acc, e| Self {
            count: acc.count + 1,
            total_ms: acc.total_ms + e.duration_ms,
            max_ms: acc.max_ms.max(e.duration_ms),
        })
    }

    /// Integer mean; 0 for an empty summary.
    pub fn mean_ms(&self) -> u64 {
        self.total_ms.checked_div(self.count).unwrap_or(0)
    }
}
