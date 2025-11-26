use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

const MAX_HISTORY_SIZE: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub query:             String,
    pub timestamp:         u64,
    pub execution_time_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct History {
    entries:   Vec<HistoryEntry>,
    #[serde(skip)]
    path:      PathBuf,
    #[serde(skip)]
    nav_index: Option<usize>, // None = not navigating, Some(i) = at history[i]
    #[serde(skip)]
    draft:     String, // Saves current input when starting navigation
}

impl Default for History {
    fn default() -> Self {
        Self {
            entries:   Vec::new(),
            path:      Self::history_path().unwrap_or_default(),
            nav_index: None,
            draft:     String::new(),
        }
    }
}

impl History {
    pub fn load() -> Result<Self> {
        let path = Self::history_path()?;

        if path.exists() {
            let content = fs::read_to_string(&path)?;
            let mut history: History = serde_json::from_str(&content)?;
            history.path = path;
            history.nav_index = None;
            history.draft = String::new();
            Ok(history)
        } else {
            Ok(Self { entries: Vec::new(), path, nav_index: None, draft: String::new() })
        }
    }

    pub fn add(&mut self, query: String, execution_time_ms: Option<u64>) -> Result<()> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

        self.entries.push(HistoryEntry { query, timestamp, execution_time_ms });

        if self.entries.len() > MAX_HISTORY_SIZE {
            self.entries.remove(0);
        }

        self.save()
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&self.path, content)?;
        Ok(())
    }

    /// Navigate to previous (older) history entry. Returns the query to display.
    /// On first call, saves current_input as draft.
    pub fn nav_prev(&mut self, current_input: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }

        match self.nav_index {
            None => {
                // Start navigating from the end (most recent)
                self.draft = current_input.to_string();
                self.nav_index = Some(self.entries.len() - 1);
            }
            Some(i) if i > 0 => {
                self.nav_index = Some(i - 1);
            }
            Some(_) => {
                // Already at oldest entry
                return self.entries.first().map(|e| e.query.as_str());
            }
        }

        self.nav_index.and_then(|i| self.entries.get(i).map(|e| e.query.as_str()))
    }

    /// Navigate to next (newer) history entry. Returns the query to display.
    /// If at the end, returns to the draft.
    pub fn nav_next(&mut self) -> Option<&str> {
        match self.nav_index {
            None => None, // Not navigating
            Some(i) => {
                if i + 1 < self.entries.len() {
                    self.nav_index = Some(i + 1);
                    self.entries.get(i + 1).map(|e| e.query.as_str())
                } else {
                    // Return to draft
                    self.nav_index = None;
                    Some(self.draft.as_str())
                }
            }
        }
    }

    /// Reset navigation state (call when user modifies input or executes)
    pub fn reset_nav(&mut self) {
        self.nav_index = None;
        self.draft.clear();
    }

    fn history_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Failed to determine config directory"))?;
        Ok(config_dir.join("test-client").join("history.json"))
    }
}
