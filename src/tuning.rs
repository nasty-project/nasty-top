//! Tuning panel state — sysfs option editing and marker snapshots.

use crate::sysfs::{self, BcachefsFs};
use std::collections::HashMap;

/// A saved configuration snapshot (marker).
#[derive(Debug, Clone)]
pub struct Marker {
    pub label: String,
    pub options: HashMap<String, String>,
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
    /// Saved markers (up to 9).
    pub markers: [Option<Marker>; 9],
}

impl TuningState {
    pub fn new(options: &HashMap<String, String>) -> Self {
        let mut option_names: Vec<String> = options.keys().cloned().collect();
        option_names.sort();
        Self {
            option_names,
            selected: 0,
            editing: false,
            edit_buf: String::new(),
            markers: Default::default(),
        }
    }

    pub fn refresh_names(&mut self, options: &HashMap<String, String>) {
        let mut names: Vec<String> = options.keys().cloned().collect();
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

    /// Save current options as a marker.
    pub fn save_marker(&mut self, slot: usize, options: &HashMap<String, String>) {
        if slot < 9 {
            self.markers[slot] = Some(Marker {
                label: format!("Marker {}", slot + 1),
                options: options.clone(),
            });
        }
    }

    /// Restore a marker — writes all options back to sysfs.
    pub fn restore_marker(&self, slot: usize, fs: &BcachefsFs) -> Result<(), String> {
        let marker = self.markers.get(slot)
            .and_then(|m| m.as_ref())
            .ok_or_else(|| format!("marker {} is empty", slot + 1))?;

        let mut errors = Vec::new();
        for (name, value) in &marker.options {
            if let Err(e) = sysfs::write_option(fs, name, value) {
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
