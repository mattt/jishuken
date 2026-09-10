//! Commit staged files with a rollback journal.
//! A journal left by an interrupted operation is restored before the next open or write.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::Change;
use crate::error::{Error, Result};

const JOURNAL: &str = "transaction.json";

pub(super) enum LogUpdate {
    Append(String),
    Replace(String),
}

#[derive(Debug, Serialize, Deserialize)]
enum LogBefore {
    Length(u64),
    Contents(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    changes: Vec<Change>,
    log_before: LogBefore,
}

pub(super) fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// Replace one file without exposing a truncated or partially written value.
fn replace(path: &Path, contents: Option<&str>) -> Result<()> {
    if read_optional(path)?.as_deref() == contents {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::Store("file has no parent".into()))?;
    if let Some(text) = contents {
        create_dir(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(text.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| e.error)?;
    } else {
        std::fs::remove_file(path)?;
    }
    sync_dir(parent)
}

fn create_dir(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::Store("directory has no parent".into()))?;
    create_dir(parent)?;
    std::fs::create_dir(path)?;
    sync_dir(parent)
}

fn sync_dir(path: &Path) -> Result<()> {
    // Windows does not support opening a directory with File::open.
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Restore an incomplete operation. The caller must hold the store lock.
pub(super) fn recover(root: &Path) -> Result<()> {
    let path = root.join(JOURNAL);
    let Some(text) = read_optional(&path)? else {
        return Ok(());
    };
    let journal: Journal = serde_json::from_str(&text)?;
    for change in journal.changes.iter().rev() {
        replace(&root.join(&change.path), change.before.as_deref())?;
    }
    let log = root.join("ops.jsonl");
    match journal.log_before {
        LogBefore::Length(len) => {
            let file = std::fs::OpenOptions::new().write(true).open(log)?;
            if file.metadata()?.len() < len {
                return Err(Error::Store(
                    "operation log is shorter than the recovery journal".into(),
                ));
            }
            file.set_len(len)?;
            file.sync_all()?;
        }
        LogBefore::Contents(text) => replace(&log, Some(&text))?,
    }
    std::fs::remove_file(path)?;
    sync_dir(root)
}

/// Commit files and history together. Normal writes only append to the log.
pub(super) fn commit(root: &Path, changes: Vec<Change>, update: LogUpdate) -> Result<()> {
    // Check for stale staged writes before creating a journal or touching any files.
    for change in &changes {
        if read_optional(&root.join(&change.path))? != change.before {
            return Err(Error::Store(format!(
                "{} changed before commit; retry the operation",
                change.path
            )));
        }
    }
    let log_path = root.join("ops.jsonl");
    // Open the log first: an unwritable log must not change fact files.
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_before = match &update {
        LogUpdate::Append(_) => LogBefore::Length(log.metadata()?.len()),
        LogUpdate::Replace(_) => LogBefore::Contents(std::fs::read_to_string(&log_path)?),
    };
    let journal = Journal {
        changes,
        log_before,
    };
    let journal_path = root.join(JOURNAL);
    replace(&journal_path, Some(&serde_json::to_string(&journal)?))?;

    let result = (|| -> Result<()> {
        for change in &journal.changes {
            replace(&root.join(&change.path), change.after.as_deref())?;
        }
        match update {
            LogUpdate::Append(text) => {
                log.write_all(text.as_bytes())?;
                log.sync_all()?;
            }
            LogUpdate::Replace(text) => replace(&log_path, Some(&text))?,
        }
        sync_dir(root)?;
        std::fs::remove_file(&journal_path)?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(recovery) = recover(root) {
            return Err(Error::Store(format!(
                "commit failed: {error}; rollback failed: {recovery}; repair the I/O error and reopen the store to recover"
            )));
        }
        return Err(error);
    }
    sync_dir(root).map_err(|e| {
        Error::Store(format!(
            "operation committed, but directory sync failed: {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::JishukenStore;

    #[test]
    fn process_exit_releases_lock_and_recovers() {
        const CHILD_STORE: &str = "JISHUKEN_CRASH_TEST_STORE";
        if let Some(root) = std::env::var_os(CHILD_STORE) {
            let root = std::path::PathBuf::from(root);
            let store = JishukenStore::open(&root).unwrap();
            let _lock = store.lock().unwrap();
            let log = std::fs::read_to_string(root.join("ops.jsonl")).unwrap();
            let journal = Journal {
                changes: vec![Change {
                    path: "progress.txt".into(),
                    before: Some("old".into()),
                    after: Some("new".into()),
                }],
                log_before: LogBefore::Length(log.len() as u64),
            };
            replace(
                &root.join(JOURNAL),
                Some(&serde_json::to_string(&journal).unwrap()),
            )
            .unwrap();
            replace(&root.join("progress.txt"), Some("new")).unwrap();
            std::fs::write(root.join("ops.jsonl"), format!("{log}{{partial")).unwrap();
            // Exit without running destructors, as with an interrupted writer.
            std::process::exit(0);
        }
        let dir = tempfile::tempdir().unwrap();
        JishukenStore::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("progress.txt"), "old").unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::transaction::tests::process_exit_releases_lock_and_recovers",
            ])
            .env(CHILD_STORE, dir.path())
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(dir.path().join(JOURNAL).is_file());
        let store = JishukenStore::open(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("progress.txt")).unwrap(),
            "old"
        );
        assert_eq!(store.op_log(None).unwrap().len(), 1);
    }

    #[test]
    fn a_later_file_failure_rolls_back_earlier_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        JishukenStore::init(root).unwrap();
        std::fs::write(root.join("first.txt"), "old").unwrap();
        std::fs::write(root.join("blocked"), "a file, not a directory").unwrap();
        let log = std::fs::read(root.join("ops.jsonl")).unwrap();
        let changes = vec![
            Change {
                path: "first.txt".into(),
                before: Some("old".into()),
                after: Some("new".into()),
            },
            Change {
                path: "blocked/second.txt".into(),
                before: None,
                after: Some("second".into()),
            },
        ];
        assert!(commit(root, changes, LogUpdate::Append("unused\n".into())).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("first.txt")).unwrap(),
            "old"
        );
        assert_eq!(std::fs::read(root.join("ops.jsonl")).unwrap(), log);
        assert!(!root.join(JOURNAL).exists());
        JishukenStore::open(root).unwrap();
    }

    #[test]
    fn reopening_rolls_back_interrupted_file_and_log_writes() {
        // Stop before the log, during append, and after a complete appended record.
        for suffix in ["", "{\"partial\":", "{\"uncommitted\":true}\n"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            JishukenStore::init(root).unwrap();
            let log = std::fs::read_to_string(root.join("ops.jsonl")).unwrap();
            let journal = Journal {
                changes: vec![
                    Change {
                        path: "facts/a/value.json".into(),
                        before: Some("old".into()),
                        after: Some("new".into()),
                    },
                    Change {
                        path: "calibration.jsonl".into(),
                        before: None,
                        after: Some("sample\n".into()),
                    },
                ],
                log_before: LogBefore::Length(log.len() as u64),
            };
            replace(
                &root.join(JOURNAL),
                Some(&serde_json::to_string(&journal).unwrap()),
            )
            .unwrap();
            for change in &journal.changes {
                replace(&root.join(&change.path), change.after.as_deref()).unwrap();
            }
            std::fs::write(root.join("ops.jsonl"), format!("{log}{suffix}")).unwrap();
            JishukenStore::open(root).unwrap();
            assert_eq!(
                std::fs::read_to_string(root.join("facts/a/value.json")).unwrap(),
                "old"
            );
            assert!(!root.join("calibration.jsonl").exists());
            assert_eq!(
                std::fs::read_to_string(root.join("ops.jsonl")).unwrap(),
                log
            );
            assert!(!root.join(JOURNAL).exists());
        }
    }

    #[test]
    fn reopening_rolls_back_an_interrupted_undo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        JishukenStore::init(root).unwrap();
        let log = std::fs::read_to_string(root.join("ops.jsonl")).unwrap();
        let journal = Journal {
            changes: vec![Change {
                path: "fact.txt".into(),
                before: Some("C".into()),
                after: Some("B".into()),
            }],
            log_before: LogBefore::Contents(log.clone()),
        };
        replace(
            &root.join(JOURNAL),
            Some(&serde_json::to_string(&journal).unwrap()),
        )
        .unwrap();
        replace(&root.join("fact.txt"), Some("B")).unwrap();
        replace(&root.join("ops.jsonl"), Some("")).unwrap();
        JishukenStore::open(root).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("fact.txt")).unwrap(), "C");
        assert_eq!(
            std::fs::read_to_string(root.join("ops.jsonl")).unwrap(),
            log
        );
    }
}
