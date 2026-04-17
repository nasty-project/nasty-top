//! Tuning panel state — sysfs option editing and marker snapshots.

use crate::sysfs::{self, BcachefsFs};
use std::collections::{HashMap, HashSet};

/// Options excluded from the tuning panel (see hidden_options.txt).
const HIDDEN_OPTIONS: &str = include_str!("hidden_options.txt");

fn hidden_set() -> HashSet<&'static str> {
    HIDDEN_OPTIONS
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// State for the tuning panel.
pub struct TuningState {
    /// Sorted list of option names for stable iteration order.
    pub option_names: Vec<String>,
    /// Currently selected option index.
    pub selected: usize,
    /// Whether inline editing is active.
    pub editing: bool,
    /// Edit buffer.
    pub edit_buf: String,
}

impl TuningState {
    pub fn new(options: &HashMap<String, String>) -> Self {
        let hidden = hidden_set();
        let mut option_names: Vec<String> = options
            .keys()
            .filter(|k| !hidden.contains(k.as_str()))
            .cloned()
            .collect();
        option_names.sort();
        Self {
            option_names,
            selected: 0,
            editing: false,
            edit_buf: String::new(),
        }
    }

    pub fn refresh_names(&mut self, options: &HashMap<String, String>) {
        let hidden = hidden_set();
        let mut names: Vec<String> = options
            .keys()
            .filter(|k| !hidden.contains(k.as_str()))
            .cloned()
            .collect();
        names.sort();
        self.option_names = names;
        if self.selected >= self.option_names.len() && !self.option_names.is_empty() {
            self.selected = self.option_names.len() - 1;
        }
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.option_names.get(self.selected).map(|s| s.as_str())
    }

    pub fn scroll_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        if self.selected + 1 < self.option_names.len() {
            self.selected += 1;
        }
    }

    pub fn start_edit(&mut self, current_value: &str) {
        self.editing = true;
        self.edit_buf = current_value.to_string();
    }

    pub fn cancel_edit(&mut self) {
        self.editing = false;
        self.edit_buf.clear();
    }

    /// Commit edit — writes to sysfs. Returns Ok with new value, or Err with message.
    pub fn commit_edit(&mut self, fs: &BcachefsFs) -> Result<String, String> {
        self.editing = false;
        let name = match self.selected_name() {
            Some(n) => n.to_string(),
            None => return Err("no option selected".into()),
        };
        let value = self.edit_buf.trim().to_string();
        sysfs::write_option(fs, &name, &value)?;
        self.edit_buf.clear();
        Ok(value)
    }

}
