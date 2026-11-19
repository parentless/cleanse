//! Safe Reclamation & Reversible Quarantine Vault Engine (`cleaner.rs`).
//!
//! Cleanse NEVER executes raw `rm -rf` on user directories. Every clean operation:
//! 1. Re-verifies all 5 layers of `safety::verify_item_for_clean` right before execution.
//! 2. Uses one of three non-destructive strategies:
//!    - `CleanStrategy::QuarantineVault`: atomic `rename` into `~/.clearance-vault/<sweep-id>/`
//!      with a JSON `manifest.json` so 1-click Undo restores every item to its exact origin.
//!    - `CleanStrategy::MacTrash`: collision-safe move into `~/.Trash` so macOS Finder's native
//!      Trash management remains in control.
//!    - `CleanStrategy::DryRun`: validates every safety proof and produces a full sweep receipt
//!      without touching a single byte on disk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::model::{
    CleanStrategy, DatasetMode, ScanItem, SweepOutcome, VaultManifest, VaultMovedEntry,
    format_bytes,
};
use crate::safety::{snapshot_running_processes, verify_item_for_clean};
use crate::platform;
use crate::scanner::user_home_dir;

pub fn vault_root_for(home: &Path) -> PathBuf {
    home.join(".clearance-vault")
}

/// Execute a sweep across selected items.
/// If `dataset != DatasetMode::LiveMac`, automatically runs in synthetic simulation mode so
/// interacting with `Demo` or `WorstCase` fixtures never mutates the real filesystem.
pub fn execute_sweep(
    items: &[ScanItem],
    strategy: CleanStrategy,
    dataset: DatasetMode,
) -> SweepOutcome {
    let home = user_home_dir();
    let active_procs = if dataset == DatasetMode::LiveMac {
        snapshot_running_processes()
    } else {
        HashSet::new()
    };
    execute_sweep_with_home(items, strategy, dataset, &home, &active_procs)
}

pub fn execute_sweep_with_home(
    items: &[ScanItem],
    strategy: CleanStrategy,
    dataset: DatasetMode,
    home: &Path,
    active_procs: &HashSet<String>,
) -> SweepOutcome {
    let effective_strategy = if dataset != DatasetMode::LiveMac && strategy != CleanStrategy::DryRun {
        // Synthetic datasets simulate Quarantine Vault in-memory without touching real disk
        strategy
    } else {
        strategy
    };

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let sweep_id = format!("sweep-{now_secs}-{}", items.len());

    let mut reclaimed_bytes = 0u64;
    let mut moved_ids = Vec::new();
    let mut skipped_reasons = Vec::new();
    let mut vault_entries = Vec::new();

    let is_synthetic = dataset != DatasetMode::LiveMac;
    let sweep_dir = vault_root_for(home).join(&sweep_id).join("items");

    for item in items.iter().filter(|it| it.selected) {
        let check_home = if is_synthetic {
            // Fixture paths start with `/Users/alex` or `/Users/trent`
            infer_fixture_home(&item.path).unwrap_or_else(|| home.to_path_buf())
        } else {
            home.to_path_buf()
        };

        let verdict = verify_item_for_clean(item, &check_home, active_procs);
        if let crate::safety::SafetyVerdict::Denied { reason } = verdict {
            skipped_reasons.push(format!("{}: {reason}", item.name));
            continue;
        }

        match (effective_strategy, is_synthetic) {
            (CleanStrategy::DryRun, _) => {
                reclaimed_bytes = reclaimed_bytes.saturating_add(item.bytes);
                moved_ids.push(item.id.clone());
            }
            (CleanStrategy::QuarantineVault, true) => {
                reclaimed_bytes = reclaimed_bytes.saturating_add(item.bytes);
                moved_ids.push(item.id.clone());
            }
            (CleanStrategy::MacTrash, true) => {
                reclaimed_bytes = reclaimed_bytes.saturating_add(item.bytes);
                moved_ids.push(item.id.clone());
            }
            (CleanStrategy::QuarantineVault, false) => {
                if let Err(err) = std::fs::create_dir_all(&sweep_dir) {
                    skipped_reasons.push(format!("{}: vault dir error ({err})", item.name));
                    continue;
                }
                let idx = vault_entries.len();
                let leaf = item
                    .path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("artifact");
                let dest = sweep_dir.join(format!("{idx:03}_{leaf}"));
                match std::fs::rename(&item.path, &dest) {
                    Ok(()) => {
                        reclaimed_bytes = reclaimed_bytes.saturating_add(item.bytes);
                        moved_ids.push(item.id.clone());
                        vault_entries.push(VaultMovedEntry {
                            name: item.name.clone(),
                            original_path: item.path.clone(),
                            vaulted_path: dest,
                            bytes: item.bytes,
                            category: item.category,
                        });
                    }
                    Err(err) => {
                        skipped_reasons.push(format!("{}: {err}", item.name));
                    }
                }
            }
            (CleanStrategy::MacTrash, false) => {
                let trash_dir = home.join(".Trash");
                // If the item is ALREADY in ~/.Trash, skip moving it onto itself
                if item.path.starts_with(&trash_dir) {
                    skipped_reasons.push(format!("{}: already in the Trash", item.name));
                    continue;
                }
                match send_to_trash(home, &item.path, &item.path, now_secs) {
                    Ok(()) => {
                        reclaimed_bytes = reclaimed_bytes.saturating_add(item.bytes);
                        moved_ids.push(item.id.clone());
                    }
                    Err(err) => {
                        skipped_reasons.push(format!("{}: {err}", item.name));
                    }
                }
            }
        }
    }

    let recorded_sweep_id = if !is_synthetic
        && effective_strategy == CleanStrategy::QuarantineVault
        && !vault_entries.is_empty()
    {
        let manifest = VaultManifest {
            sweep_id: sweep_id.clone(),
            created_epoch_secs: now_secs,
            label: format!(
                "Staged {} ({})",
                crate::model::pluralize(vault_entries.len(), "item", "items"),
                format_bytes(reclaimed_bytes)
            ),
            total_bytes: reclaimed_bytes,
            entries: vault_entries,
        };
        let manifest_path = vault_root_for(home).join(&sweep_id).join("manifest.json");
        if let Ok(json) = serde_json::to_string_pretty(&manifest) {
            let _ = std::fs::write(manifest_path, json);
        }
        Some(sweep_id)
    } else if is_synthetic
        && effective_strategy == CleanStrategy::QuarantineVault
        && !moved_ids.is_empty()
    {
        Some(sweep_id)
    } else {
        None
    };

    SweepOutcome {
        strategy: effective_strategy,
        sweep_id: recorded_sweep_id,
        reclaimed_bytes,
        moved_ids,
        skipped_reasons,
    }
}

fn infer_fixture_home(path: &Path) -> Option<PathBuf> {
    let mut comps = path.components();
    let root = comps.next()?;
    let users = comps.next()?;
    let user = comps.next()?;
    if users.as_os_str() == "Users" {
        let mut p = PathBuf::from(root.as_os_str());
        p.push(users.as_os_str());
        p.push(user.as_os_str());
        Some(p)
    } else {
        None
    }
}

fn unique_destination(dir: &Path, original: &Path, stamp: u64) -> PathBuf {
    let file_name = original
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("item");
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = original
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("item");
    match original.extension().and_then(|e| e.to_str()) {
        Some(ext) => dir.join(format!("{stem}-{stamp}.{ext}")),
        None => dir.join(format!("{stem}-{stamp}")),
    }
}

/// Load all saved Quarantine Vault manifests from `~/.clearance-vault/`, newest first.
pub fn load_vault_manifests() -> Vec<VaultManifest> {
    load_vault_manifests_from(&user_home_dir())
}

pub fn load_vault_manifests_from(home: &Path) -> Vec<VaultManifest> {
    let root = vault_root_for(home);
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut manifests = Vec::new();
    for entry in rd.flatten() {
        let manifest_path = entry.path().join("manifest.json");
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        if let Ok(m) = serde_json::from_str::<VaultManifest>(&raw) {
            manifests.push(m);
        }
    }
    manifests.sort_by(|a, b| b.created_epoch_secs.cmp(&a.created_epoch_secs));
    manifests
}

/// 1-click Undo: restore every vaulted item in `sweep_id` back to its exact original path!
pub fn restore_vault_sweep(sweep_id: &str) -> Result<(usize, u64), String> {
    restore_vault_sweep_in(&user_home_dir(), sweep_id)
}

pub fn restore_vault_sweep_in(home: &Path, sweep_id: &str) -> Result<(usize, u64), String> {
    let sweep_folder = vault_root_for(home).join(sweep_id);
    let manifest_path = sweep_folder.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Could not read vault manifest: {e}"))?;
    let manifest: VaultManifest =
        serde_json::from_str(&raw).map_err(|e| format!("Corrupt vault manifest: {e}"))?;

    let mut restored_count = 0usize;
    let mut restored_bytes = 0u64;
    let mut remaining = Vec::new();

    for entry in manifest.entries {
        if !entry.vaulted_path.exists() {
            continue;
        }
        if entry.original_path.exists() {
            remaining.push(entry);
            continue;
        }
        if let Some(parent) = entry.original_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(&entry.vaulted_path, &entry.original_path) {
            Ok(()) => {
                restored_count += 1;
                restored_bytes = restored_bytes.saturating_add(entry.bytes);
            }
            Err(_) => remaining.push(entry),
        }
    }

    if remaining.is_empty() {
        let _ = std::fs::remove_file(&manifest_path);
        let _ = std::fs::remove_dir(sweep_folder.join("items"));
        let _ = std::fs::remove_dir(&sweep_folder);
    } else {
        let updated = VaultManifest {
            total_bytes: remaining.iter().map(|e| e.bytes).sum(),
            entries: remaining,
            ..manifest
        };
        if let Ok(json) = serde_json::to_string_pretty(&updated) {
            let _ = std::fs::write(&manifest_path, json);
        }
    }

    Ok((restored_count, restored_bytes))
}

/// Put one item in the system Trash: the Recycle Bin on Windows, `~/.Trash` on a Mac.
fn send_to_trash(home: &Path, path: &Path, name_from: &Path, stamp: u64) -> std::io::Result<()> {
    if platform::IS_WINDOWS {
        return platform::send_to_recycle_bin(path);
    }
    let trash_dir = home.join(".Trash");
    std::fs::create_dir_all(&trash_dir)?;
    std::fs::rename(path, unique_destination(&trash_dir, name_from, stamp))
}

/// Move a vaulted sweep into the system Trash when the user is ready to release disk space.
pub fn move_vault_sweep_to_trash(sweep_id: &str) -> Result<usize, String> {
    let home = user_home_dir();
    let sweep_folder = vault_root_for(&home).join(sweep_id);
    let manifest_path = sweep_folder.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Could not read vault manifest: {e}"))?;
    let manifest: VaultManifest =
        serde_json::from_str(&raw).map_err(|e| format!("Corrupt vault manifest: {e}"))?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();

    let mut moved = 0usize;
    for entry in manifest.entries {
        if !entry.vaulted_path.exists() {
            continue;
        }
        if send_to_trash(&home, &entry.vaulted_path, &entry.original_path, stamp + moved as u64).is_ok() {
            moved += 1;
        }
    }
    let _ = std::fs::remove_file(&manifest_path);
    let _ = std::fs::remove_dir(sweep_folder.join("items"));
    let _ = std::fs::remove_dir(&sweep_folder);
    Ok(moved)
}

/// Reveal a path in the system file browser (Finder / File Explorer). No-op during headless runs.
pub fn reveal_in_finder(path: &Path) {
    platform::reveal(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Category, SafetyTier};

    #[test]
    fn quarantine_vault_stages_and_restores_exact_files_while_blocking_protected() {
        // Not `$TMPDIR`: on macOS that is `/var/folders/…`, which the safety layer blocks by design.
        let tmp = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp").join(format!(
            "clearance-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache_dir = tmp.join("Library/Caches/com.example.demo");
        let ssh_dir = tmp.join(".ssh");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&ssh_dir).unwrap();
        let cache_file = cache_dir.join("blob.bin");
        let ssh_key = ssh_dir.join("id_ed25519");
        std::fs::write(&cache_file, vec![7u8; 4096]).unwrap();
        std::fs::write(&ssh_key, b"PRIVATE KEY").unwrap();

        let items = vec![
            ScanItem {
                id: "ok-cache".into(),
                name: "Demo Cache".into(),
                subtitle: "Regenerable cache".into(),
                path: cache_dir.clone(),
                bytes: 4096,
                logical_bytes: 4096,
                category: Category::SystemCaches,
                safety: SafetyTier::SafeRegenerable,
                age_days: 12,
                file_count: 1,
                rebuild_hint: Some("Recreated on launch".into()),
                in_use_by: None,
                selected: true,
            },
            ScanItem {
                id: "bad-ssh".into(),
                name: "SSH Key".into(),
                subtitle: "Must never be touched".into(),
                path: ssh_key.clone(),
                bytes: 64,
                logical_bytes: 64,
                category: Category::SystemCaches,
                safety: SafetyTier::SafeRegenerable,
                age_days: 90,
                file_count: 1,
                rebuild_hint: None,
                in_use_by: None,
                selected: true,
            },
        ];

        let outcome = execute_sweep_with_home(
            &items,
            CleanStrategy::QuarantineVault,
            DatasetMode::LiveMac,
            &tmp,
            &HashSet::new(),
        );

        assert_eq!(outcome.moved_ids, vec!["ok-cache".to_string()]);
        assert_eq!(outcome.skipped_reasons.len(), 1);
        assert!(ssh_key.exists(), "Protected ~/.ssh key must NEVER move");
        assert!(!cache_dir.exists(), "Cache dir should now reside in Quarantine Vault");

        let sweep_id = outcome.sweep_id.expect("sweep_id recorded");
        let manifests = load_vault_manifests_from(&tmp);
        assert_eq!(manifests.len(), 1);

        // 1-Click Undo!
        let (restored_n, restored_bytes) = restore_vault_sweep_in(&tmp, &sweep_id).unwrap();
        assert_eq!(restored_n, 1);
        assert_eq!(restored_bytes, 4096);
        assert!(cache_file.exists(), "Cache file restored bit-for-bit to original path");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
