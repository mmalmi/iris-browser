//! History storage and search using heed (LMDB)
//!
//! Stores navigation history for fuzzy search suggestions.

use heed::types::{Bytes, Str};
use heed::{Database, Env, EnvOpenOptions};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::debug;

const MAX_HISTORY_ENTRIES: usize = 1000;
const LMDB_MAX_READERS: u32 = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub path: String,
    pub label: String,
    pub entry_type: String,
    pub npub: Option<String>,
    pub tree_name: Option<String>,
    pub visit_count: u32,
    pub last_visited: u64,
    pub first_visited: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorySearchResult {
    pub entry: HistoryEntry,
    pub score: f64,
}

pub struct HistoryStore {
    env: Env,
    db: Database<Str, Bytes>,
    entry_count: RwLock<usize>,
}

impl HistoryStore {
    pub fn new(data_dir: &Path) -> Result<Self, String> {
        let history_dir = data_dir.join("history");
        std::fs::create_dir_all(&history_dir)
            .map_err(|e| format!("Failed to create history dir: {}", e))?;

        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(10 * 1024 * 1024)
                .max_dbs(1)
                .max_readers(LMDB_MAX_READERS)
                .open(&history_dir)
                .map_err(|e| format!("Failed to open history db: {}", e))?
        };
        if let Ok(cleared) = env.clear_stale_readers() {
            if cleared > 0 {
                debug!("Cleared {} stale LMDB readers for history store", cleared);
            }
        }

        let mut wtxn = env
            .write_txn()
            .map_err(|e| format!("Failed to start txn: {}", e))?;
        let db = env
            .create_database(&mut wtxn, Some("history"))
            .map_err(|e| format!("Failed to create db: {}", e))?;
        wtxn.commit()
            .map_err(|e| format!("Failed to commit: {}", e))?;

        let count = {
            let rtxn = env
                .read_txn()
                .map_err(|e| format!("Failed to start read txn: {}", e))?;
            db.len(&rtxn).unwrap_or(0) as usize
        };

        Ok(Self {
            env,
            db,
            entry_count: RwLock::new(count),
        })
    }

    pub fn record_visit(&self, entry: HistoryEntry) -> Result<(), String> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| format!("Failed to start write txn: {}", e))?;

        let existing: Option<HistoryEntry> = self
            .db
            .get(&wtxn, &entry.path)
            .map_err(|e| format!("Failed to get: {}", e))?
            .and_then(|bytes| bincode::deserialize(bytes).ok());

        let updated_entry = if let Some(mut existing) = existing {
            existing.label = entry.label;
            existing.visit_count += 1;
            existing.last_visited = entry.last_visited;
            existing
        } else {
            let count = *self.entry_count.read();
            if count >= MAX_HISTORY_ENTRIES {
                self.evict_oldest(&mut wtxn)?;
            }
            *self.entry_count.write() += 1;
            entry
        };

        let bytes = bincode::serialize(&updated_entry)
            .map_err(|e| format!("Failed to serialize: {}", e))?;
        self.db
            .put(&mut wtxn, &updated_entry.path, &bytes)
            .map_err(|e| format!("Failed to put: {}", e))?;
        wtxn.commit()
            .map_err(|e| format!("Failed to commit: {}", e))?;

        debug!("Recorded history visit: {}", updated_entry.path);
        Ok(())
    }

    fn evict_oldest(&self, wtxn: &mut heed::RwTxn) -> Result<(), String> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| format!("Failed to read: {}", e))?;

        let mut entries: Vec<(String, u64)> = Vec::new();
        let iter = self
            .db
            .iter(&rtxn)
            .map_err(|e| format!("Failed to iterate: {}", e))?;

        for item in iter {
            let (key, value) = item.map_err(|e| format!("Iter error: {}", e))?;
            if let Ok(entry) = bincode::deserialize::<HistoryEntry>(value) {
                entries.push((key.to_string(), entry.last_visited));
            }
        }
        drop(rtxn);

        entries.sort_by_key(|(_, ts)| *ts);
        let to_remove = entries.len() / 10;
        for (path, _) in entries.into_iter().take(to_remove.max(1)) {
            self.db
                .delete(wtxn, &path)
                .map_err(|e| format!("Failed to delete: {}", e))?;
            *self.entry_count.write() -= 1;
        }

        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistorySearchResult>, String> {
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| format!("Failed to start read txn: {}", e))?;

        let query_lower = query.to_lowercase();
        let mut results: Vec<HistorySearchResult> = Vec::new();

        let iter = self
            .db
            .iter(&rtxn)
            .map_err(|e| format!("Failed to iterate: {}", e))?;

        for item in iter {
            let (_key, value) = item.map_err(|e| format!("Iter error: {}", e))?;
            if let Ok(entry) = bincode::deserialize::<HistoryEntry>(value) {
                let score = fuzzy_score(&query_lower, &entry);
                if score > 0.0 {
                    results.push(HistorySearchResult { entry, score });
                }
            }
        }

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.entry.last_visited.cmp(&a.entry.last_visited))
        });

        results.truncate(limit);
        Ok(results)
    }

    pub fn delete_entry(&self, path: &str) -> Result<bool, String> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| format!("Failed to start write txn: {}", e))?;

        let existed = self
            .db
            .delete(&mut wtxn, path)
            .map_err(|e| format!("Failed to delete: {}", e))?;

        wtxn.commit()
            .map_err(|e| format!("Failed to commit: {}", e))?;

        if existed {
            *self.entry_count.write() -= 1;
            debug!("Deleted history entry: {}", path);
        }

        Ok(existed)
    }

    pub fn clear(&self) -> Result<(), String> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| format!("Failed to start write txn: {}", e))?;

        self.db
            .clear(&mut wtxn)
            .map_err(|e| format!("Failed to clear: {}", e))?;

        wtxn.commit()
            .map_err(|e| format!("Failed to commit: {}", e))?;

        *self.entry_count.write() = 0;
        debug!("Cleared all history entries");

        Ok(())
    }

    pub fn get_recent(&self, limit: usize) -> Result<Vec<HistoryEntry>, String> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| format!("Failed to start read txn: {}", e))?;

        let mut entries: Vec<HistoryEntry> = Vec::new();
        let iter = self
            .db
            .iter(&rtxn)
            .map_err(|e| format!("Failed to iterate: {}", e))?;

        for item in iter {
            let (_key, value) = item.map_err(|e| format!("Iter error: {}", e))?;
            if let Ok(entry) = bincode::deserialize::<HistoryEntry>(value) {
                entries.push(entry);
            }
        }

        entries.sort_by(|a, b| b.last_visited.cmp(&a.last_visited));
        entries.truncate(limit);
        Ok(entries)
    }
}

fn fuzzy_score(query: &str, entry: &HistoryEntry) -> f64 {
    let mut max_score: f64 = 0.0;
    let label_lower = entry.label.to_lowercase();
    max_score = max_score.max(fuzzy_match_string(query, &label_lower));
    let path_lower = entry.path.to_lowercase();
    max_score = max_score.max(fuzzy_match_string(query, &path_lower) * 0.8);
    if let Some(ref tree_name) = entry.tree_name {
        let tree_lower = tree_name.to_lowercase();
        max_score = max_score.max(fuzzy_match_string(query, &tree_lower) * 0.7);
    }
    let freq_boost = (entry.visit_count as f64).ln_1p() * 0.1;
    max_score + freq_boost
}

fn fuzzy_match_string(query: &str, target: &str) -> f64 {
    if query.is_empty() || target.is_empty() {
        return 0.0;
    }
    if target == query {
        return 10.0;
    }
    if target.starts_with(query) {
        return 8.0 + (query.len() as f64 / target.len() as f64);
    }
    if target.contains(query) {
        return 5.0 + (query.len() as f64 / target.len() as f64);
    }
    for word in target.split(|c: char| !c.is_alphanumeric()) {
        if word.starts_with(query) {
            return 4.0 + (query.len() as f64 / word.len() as f64);
        }
    }

    let query_chars: Vec<char> = query.chars().collect();
    let target_chars: Vec<char> = target.chars().collect();
    let mut query_idx = 0;
    let mut score = 0.0;
    let mut prev_match_idx: Option<usize> = None;

    for (target_idx, &target_char) in target_chars.iter().enumerate() {
        if query_idx < query_chars.len() && target_char == query_chars[query_idx] {
            if let Some(prev) = prev_match_idx {
                if target_idx == prev + 1 {
                    score += 0.5;
                }
            }
            if target_idx == 0 || !target_chars[target_idx - 1].is_alphanumeric() {
                score += 0.3;
            }
            score += 0.2;
            prev_match_idx = Some(target_idx);
            query_idx += 1;
        }
    }

    if query_idx == query_chars.len() {
        score
    } else {
        0.0
    }
}

// Tauri Commands

#[tauri::command]
pub fn record_history_visit(
    path: String,
    label: String,
    entry_type: String,
    npub: Option<String>,
    tree_name: Option<String>,
    history: tauri::State<'_, Arc<HistoryStore>>,
) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let entry = HistoryEntry {
        path,
        label,
        entry_type,
        npub,
        tree_name,
        visit_count: 1,
        last_visited: now,
        first_visited: now,
    };

    history.record_visit(entry)
}

#[tauri::command]
pub fn search_history(
    query: String,
    limit: usize,
    history: tauri::State<'_, Arc<HistoryStore>>,
) -> Result<Vec<HistorySearchResult>, String> {
    history.search(&query, limit)
}

#[tauri::command]
pub fn delete_history_entry(
    path: String,
    history: tauri::State<'_, Arc<HistoryStore>>,
) -> Result<bool, String> {
    history.delete_entry(&path)
}

#[tauri::command]
pub fn clear_history(history: tauri::State<'_, Arc<HistoryStore>>) -> Result<(), String> {
    history.clear()
}

#[tauri::command]
pub fn get_recent_history(
    limit: usize,
    history: tauri::State<'_, Arc<HistoryStore>>,
) -> Result<Vec<HistoryEntry>, String> {
    history.get_recent(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_fuzzy_match_exact() {
        assert!(fuzzy_match_string("hello", "hello") > 9.0);
    }

    #[test]
    fn test_fuzzy_match_prefix() {
        assert!(fuzzy_match_string("hel", "hello") > 7.0);
    }

    #[test]
    fn test_fuzzy_match_no_match() {
        assert_eq!(fuzzy_match_string("xyz", "hello"), 0.0);
    }

    #[test]
    fn test_history_store_basic() {
        let dir = tempdir().unwrap();
        let store = HistoryStore::new(dir.path()).unwrap();

        let entry = HistoryEntry {
            path: "/test/path".to_string(),
            label: "Test Entry".to_string(),
            entry_type: "tree".to_string(),
            npub: None,
            tree_name: Some("test".to_string()),
            visit_count: 1,
            last_visited: 1234567890,
            first_visited: 1234567890,
        };

        store.record_visit(entry).unwrap();
        let results = store.search("test", 10).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].entry.path, "/test/path");
    }

    #[test]
    fn test_delete_entry() {
        let dir = tempdir().unwrap();
        let store = HistoryStore::new(dir.path()).unwrap();

        let entry = HistoryEntry {
            path: "/delete/me".to_string(),
            label: "Delete Me".to_string(),
            entry_type: "tree".to_string(),
            npub: None,
            tree_name: None,
            visit_count: 1,
            last_visited: 1000,
            first_visited: 1000,
        };

        store.record_visit(entry).unwrap();
        assert_eq!(store.get_recent(10).unwrap().len(), 1);

        let deleted = store.delete_entry("/delete/me").unwrap();
        assert!(deleted);
        assert_eq!(store.get_recent(10).unwrap().len(), 0);

        // Deleting non-existent entry returns false
        let deleted_again = store.delete_entry("/delete/me").unwrap();
        assert!(!deleted_again);
    }

    #[test]
    fn test_clear() {
        let dir = tempdir().unwrap();
        let store = HistoryStore::new(dir.path()).unwrap();

        for i in 0..5 {
            let entry = HistoryEntry {
                path: format!("/path/{}", i),
                label: format!("Entry {}", i),
                entry_type: "tree".to_string(),
                npub: None,
                tree_name: None,
                visit_count: 1,
                last_visited: 1000 + i as u64,
                first_visited: 1000 + i as u64,
            };
            store.record_visit(entry).unwrap();
        }

        assert_eq!(store.get_recent(10).unwrap().len(), 5);

        store.clear().unwrap();
        assert_eq!(store.get_recent(10).unwrap().len(), 0);

        // Can add entries after clear
        let entry = HistoryEntry {
            path: "/after/clear".to_string(),
            label: "After Clear".to_string(),
            entry_type: "tree".to_string(),
            npub: None,
            tree_name: None,
            visit_count: 1,
            last_visited: 2000,
            first_visited: 2000,
        };
        store.record_visit(entry).unwrap();
        assert_eq!(store.get_recent(10).unwrap().len(), 1);
    }

    #[test]
    fn test_history_visit_count() {
        let dir = tempdir().unwrap();
        let store = HistoryStore::new(dir.path()).unwrap();

        let entry = HistoryEntry {
            path: "/test".to_string(),
            label: "Test".to_string(),
            entry_type: "tree".to_string(),
            npub: None,
            tree_name: None,
            visit_count: 1,
            last_visited: 1000,
            first_visited: 1000,
        };

        store.record_visit(entry.clone()).unwrap();
        store.record_visit(entry.clone()).unwrap();
        store.record_visit(entry).unwrap();

        let recent = store.get_recent(10).unwrap();
        assert_eq!(recent[0].visit_count, 3);
    }
}
