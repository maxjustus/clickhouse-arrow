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
    entries:        Vec<HistoryEntry>,
    #[serde(skip)]
    path:           PathBuf,
    #[serde(skip)]
    nav_index:      Option<usize>, // None = not navigating, Some(i) = at history[i]
    #[serde(skip)]
    draft:          String, // Saves current input when starting navigation
    // Search mode (Ctrl+R)
    #[serde(skip)]
    search_active:  bool,
    #[serde(skip)]
    search_pattern: String,
    #[serde(skip)]
    search_matches: Vec<usize>, // indices into entries (newest first)
    #[serde(skip)]
    search_index:   usize, // index into search_matches
}

impl Default for History {
    fn default() -> Self {
        Self {
            entries:        Vec::new(),
            path:           Self::history_path().unwrap_or_default(),
            nav_index:      None,
            draft:          String::new(),
            search_active:  false,
            search_pattern: String::new(),
            search_matches: Vec::new(),
            search_index:   0,
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
            history.search_active = false;
            history.search_pattern = String::new();
            history.search_matches = Vec::new();
            history.search_index = 0;
            Ok(history)
        } else {
            Ok(Self::default())
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

    // --- Search mode (Ctrl+R) ---

    pub fn start_search(&mut self, current: &str) {
        self.draft = current.to_string();
        self.search_active = true;
        self.search_pattern.clear();
        // All entries match empty pattern, newest first
        self.search_matches = (0..self.entries.len()).rev().collect();
        self.search_index = 0;
    }

    pub fn update_search(&mut self, pattern: &str) {
        self.search_pattern = pattern.to_string();
        let lower = pattern.to_lowercase();
        self.search_matches = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.query.to_lowercase().contains(&lower))
            .map(|(i, _)| i)
            .rev() // newest first
            .collect();
        self.search_index = 0;
    }

    pub fn search_prev(&mut self) -> Option<&str> {
        if self.search_matches.is_empty() {
            return None;
        }
        if self.search_index + 1 < self.search_matches.len() {
            self.search_index += 1;
        }
        self.current_search_result()
    }

    pub fn search_next(&mut self) -> Option<&str> {
        if self.search_matches.is_empty() {
            return None;
        }
        if self.search_index > 0 {
            self.search_index -= 1;
        }
        self.current_search_result()
    }

    pub fn current_search_result(&self) -> Option<&str> {
        self.search_matches
            .get(self.search_index)
            .and_then(|&i| self.entries.get(i))
            .map(|e| e.query.as_str())
    }

    pub fn cancel_search(&mut self) -> &str {
        self.end_search();
        &self.draft
    }

    pub fn end_search(&mut self) {
        self.search_active = false;
        self.search_pattern.clear();
        self.search_matches.clear();
        self.search_index = 0;
    }

    pub fn is_searching(&self) -> bool { self.search_active }

    pub fn search_pattern(&self) -> &str { &self.search_pattern }

    pub fn search_match_count(&self) -> usize { self.search_matches.len() }

    pub fn search_match_position(&self) -> usize {
        if self.search_matches.is_empty() { 0 } else { self.search_index + 1 }
    }

    fn history_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Failed to determine config directory"))?;
        Ok(config_dir.join("test-client").join("history.json"))
    }
}
