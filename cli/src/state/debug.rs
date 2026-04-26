use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugChannelState {
    max_entries: usize,
    entries: VecDeque<String>,
}

impl Default for DebugChannelState {
    fn default() -> Self {
        Self {
            max_entries: 256,
            entries: VecDeque::new(),
        }
    }
}

impl DebugChannelState {
    pub fn push(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        if entry.trim().is_empty() {
            return;
        }
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &String> {
        self.entries.iter()
    }
}
