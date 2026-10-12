use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Tracks which Skills are "loaded" for the current run (i.e. the model has
/// asked for the full instructions). Cheap to clone; backed by `Arc<Mutex<>>`.
#[derive(Default, Clone, Debug)]
pub struct LoadedSkillsState {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default, Debug)]
struct Inner {
    loaded: BTreeSet<String>,
    invocation_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedSkillsSnapshot {
    pub loaded: Vec<String>,
    pub invocation_count: u32,
}

impl LoadedSkillsState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a skill as loaded. Returns true if this was a new entry.
    pub fn mark_loaded(&self, name: &str) -> bool {
        let mut g = self.inner.lock().expect("loaded-skills mutex");
        g.invocation_count += 1;
        g.loaded.insert(name.to_string())
    }

    pub fn count(&self) -> usize {
        self.inner.lock().expect("loaded-skills mutex").loaded.len()
    }

    pub fn invocation_count(&self) -> u32 {
        self.inner
            .lock()
            .expect("loaded-skills mutex")
            .invocation_count
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        self.inner
            .lock()
            .expect("loaded-skills mutex")
            .loaded
            .contains(name)
    }

    pub fn snapshot(&self) -> LoadedSkillsSnapshot {
        let g = self.inner.lock().expect("loaded-skills mutex");
        LoadedSkillsSnapshot {
            loaded: g.loaded.iter().cloned().collect(),
            invocation_count: g.invocation_count,
        }
    }
}
