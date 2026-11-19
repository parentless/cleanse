//! Parallel, non-blocking storage & regenerable artifact scanner for macOS and Windows (`scanner.rs`).
//!
//! Reads real volume capacity, inspects installed application folders and
//! running processes, and walks safe user trees without following symlinks or touching protected
//! personal/credential vaults. Computes both physical APFS allocated bytes (`st_blocks * 512`)
//! and apparent logical size (`st_size`) so sparse disk images (`Docker.raw`, VM disks) never lie.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rune::Waker;

use crate::platform;
use crate::model::{Category, DiskOverview, SafetyTier, ScanItem};
use crate::safety::{
    INSTALLER_EXTENSIONS, default_selected, matching_running_process, snapshot_running_processes,
    verify_build_artifact_sentinel, verify_path_constitution,
};

#[derive(Clone, Debug)]
pub enum ScanMessage {
    Disk(DiskOverview),
    Progress { phase: String, scanned_dirs: usize },
    Item(ScanItem),
    Finished { elapsed_ms: u64 },
}

pub struct ScanHandle {
    pub rx: Receiver<ScanMessage>,
    cancel: Arc<AtomicBool>,
}

impl ScanHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for ScanHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Resolve the current user's home directory safely.
pub fn user_home_dir() -> PathBuf {
    platform::home_dir()
}

/// Query real volume capacity: APFS `/System/Volumes/Data` on a Mac, the system drive on Windows.
pub fn query_disk_overview() -> DiskOverview {
    let mut candidates = vec![platform::system_volume_root()];
    if !platform::IS_WINDOWS {
        candidates.push(PathBuf::from("/"));
    }
    for mount in candidates {
        let Some((total_bytes, free_avail, free_bfree)) = platform::capacity(&mount) else {
            continue;
        };
        if platform::IS_WINDOWS {
            return DiskOverview {
                volume_name: format!("Local Disk ({})", mount.display().to_string().trim_end_matches('\\')),
                mount_point: mount.display().to_string(),
                filesystem: "NTFS".into(),
                total_bytes,
                free_bytes: free_avail,
                purgeable_bytes: 0,
                local_snapshots: 0,
            };
        }
        // Difference between superuser free blocks and unprivileged available blocks + APFS snapshot reserve estimate
        let snapshot_count = count_local_tm_snapshots();
        let reserved_delta = free_bfree.saturating_sub(free_avail);
        let purgeable_bytes = if reserved_delta > 256_000_000 {
            reserved_delta
        } else {
            (snapshot_count as u64) * 2_400_000_000
        };
        return DiskOverview {
            volume_name: "Macintosh HD".into(),
            mount_point: mount.display().to_string(),
            filesystem: "APFS".into(),
            total_bytes,
            free_bytes: free_avail,
            purgeable_bytes,
            local_snapshots: snapshot_count,
        };
    }
    DiskOverview::default()
}

fn count_local_tm_snapshots() -> usize {
    platform::local_snapshot_names("/")
        .iter()
        .filter(|n| n.contains("com.apple.TimeMachine"))
        .count()
}

/// Spawn a non-blocking background scan of the live Mac, waking Rune's event loop as findings stream in.
pub fn start_live_scan(waker: Option<Waker>) -> ScanHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = cancel.clone();

    std::thread::Builder::new()
        .name("clearance-scanner".into())
        .spawn(move || {
            run_scan_worker(tx, cancel_worker, waker);
        })
        .expect("spawn clearance scanner thread");

    ScanHandle { rx, cancel }
}

fn notify(tx: &Sender<ScanMessage>, waker: &Option<Waker>, msg: ScanMessage) -> bool {
    if tx.send(msg).is_err() {
        return false;
    }
    if let Some(w) = waker {
        w();
    }
    true
}

fn run_scan_worker(tx: Sender<ScanMessage>, cancel: Arc<AtomicBool>, waker: Option<Waker>) {
    let started = Instant::now();
    let home = user_home_dir();
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();

    let disk = query_disk_overview();
    if !notify(&tx, &waker, ScanMessage::Disk(disk)) {
        return;
    }

    let active_procs = snapshot_running_processes();
    let installed_apps = snapshot_installed_apps(&home);
    let mut scanned_dirs = 0usize;
    let mut seq = 0usize;

    // 1. SDK & Package Manager Toolchain Hoards
    let _ = notify(
        &tx,
        &waker,
        ScanMessage::Progress {
            phase: "Inspecting SDK & package manager caches…".into(),
            scanned_dirs,
        },
    );
    scan_toolchain_hoards(
        &home,
        now_secs,
        &active_procs,
        &cancel,
        &mut scanned_dirs,
        &mut seq,
        &tx,
        &waker,
    );
    if cancel.load(Ordering::Relaxed) {
        return;
    }

    // 2. ~/Library/Caches, Logs & Orphaned App Leftovers
    let _ = notify(
        &tx,
        &waker,
        ScanMessage::Progress {
            phase: "Analyzing ~/Library/Caches & orphaned app data…".into(),
            scanned_dirs,
        },
    );
    scan_library_caches(
        &home,
        now_secs,
        &active_procs,
        &installed_apps,
        &cancel,
        &mut scanned_dirs,
        &mut seq,
        &tx,
        &waker,
    );
    if cancel.load(Ordering::Relaxed) {
        return;
    }

    // 3. Dormant Project Build Artifacts (`target/`, `node_modules/`, `.next/`, `.venv/`)
    let _ = notify(
        &tx,
        &waker,
        ScanMessage::Progress {
            phase: "Scanning developer workspaces for dormant build artifacts…".into(),
            scanned_dirs,
        },
    );
    scan_dev_workspaces(
        &home,
        now_secs,
        &cancel,
        &mut scanned_dirs,
        &mut seq,
        &tx,
        &waker,
    );
    if cancel.load(Ordering::Relaxed) {
        return;
    }

    // 4. Forgotten Installers in ~/Downloads + Large File Radar + ~/.Trash
    let _ = notify(
        &tx,
        &waker,
        ScanMessage::Progress {
            phase: "Checking Downloads installers, Trash & Large File Radar…".into(),
            scanned_dirs,
        },
    );
    scan_downloads_and_trash(
        &home,
        now_secs,
        &installed_apps,
        &cancel,
        &mut scanned_dirs,
        &mut seq,
        &tx,
        &waker,
    );

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let _ = notify(&tx, &waker, ScanMessage::Finished { elapsed_ms });
}

/// Collect normalized tokens for installed `.app` bundles so we can spot orphaned caches
/// and already-installed `.dmg` disk images!
fn snapshot_installed_apps(home: &Path) -> HashSet<String> {
    let mut apps = HashSet::new();
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        home.join("Applications"),
    ];
    if platform::IS_WINDOWS {
        roots = ["ProgramFiles", "ProgramFiles(x86)"]
            .iter()
            .filter_map(|v| std::env::var_os(v).map(PathBuf::from))
            .chain([home.join("AppData/Local/Programs")])
            .collect();
    }
    for root in roots {
        let Ok(rd) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let cleaned: String = stem
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            if cleaned.len() >= 3 {
                apps.insert(cleaned);
            }
            for tok in stem.split([' ', '-', '_']) {
                let lower = tok.to_ascii_lowercase();
                if lower.len() >= 4 {
                    apps.insert(lower);
                }
            }
        }
    }
    apps
}

#[derive(Clone, Copy, Default)]
struct DirSize {
    physical_bytes: u64,
    logical_bytes: u64,
    file_count: u64,
    newest_mtime_secs: u64,
}

/// Fast non-symlink-following directory size accumulator with cancellation and file-count cap.
fn measure_path(path: &Path, cancel: &AtomicBool, scanned_dirs: &mut usize) -> DirSize {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return DirSize::default();
    };
    if meta.file_type().is_symlink() {
        return DirSize::default();
    }
    let mtime = crate::platform::mtime_secs(&meta);
    if meta.is_file() {
        let phys = crate::platform::phys_bytes(&meta);
        let logi = meta.len();
        return DirSize {
            physical_bytes: if phys > 0 { phys } else { logi },
            logical_bytes: logi,
            file_count: 1,
            newest_mtime_secs: mtime,
        };
    }
    if !meta.is_dir() {
        return DirSize::default();
    }

    let mut total = DirSize {
        newest_mtime_secs: mtime,
        ..DirSize::default()
    };
    let mut stack = vec![(path.to_path_buf(), 0usize)];
    let max_files = 120_000u64;

    while let Some((dir, depth)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) || total.file_count >= max_files || depth > 14 {
            break;
        }
        *scanned_dirs += 1;
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            let p = entry.path();
            if ft.is_dir() {
                stack.push((p, depth + 1));
            } else if ft.is_file()
                && let Ok(m) = entry.metadata()
            {
                let phys = crate::platform::phys_bytes(&m);
                let logi = m.len();
                total.physical_bytes = total
                    .physical_bytes
                    .saturating_add(if phys > 0 { phys } else { logi });
                total.logical_bytes = total.logical_bytes.saturating_add(logi);
                total.file_count += 1;
                let fm = crate::platform::mtime_secs(&m);
                if fm > total.newest_mtime_secs {
                    total.newest_mtime_secs = fm;
                }
            }
        }
    }
    total
}

fn age_days_from_mtime(now_secs: u64, mtime_secs: u64) -> u32 {
    if mtime_secs == 0 || now_secs <= mtime_secs {
        0
    } else {
        ((now_secs - mtime_secs) / 86_400) as u32
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_toolchain_hoards(
    home: &Path,
    now_secs: u64,
    active_procs: &HashSet<String>,
    cancel: &AtomicBool,
    scanned_dirs: &mut usize,
    seq: &mut usize,
    tx: &Sender<ScanMessage>,
    waker: &Option<Waker>,
) {
    let specs: &[(&str, &str, &str, &str)] = &[
        (
            ".cargo/registry/cache",
            "Cargo Crate Archives (.crate)",
            "Downloaded crates.io tarballs cached locally",
            "Re-downloaded automatically by Cargo on demand",
        ),
        (
            ".cargo/git/checkouts",
            "Cargo Git Checkouts Cache",
            "Cloned git dependency working trees in ~/.cargo/git",
            "Re-checked out automatically by Cargo when building",
        ),
        (
            "Library/Caches/Homebrew",
            "Homebrew Bottle & Source Cache",
            "Downloaded .bottle.tar.gz installers kept after brew upgrade",
            "brew cleanup / fetched automatically on brew reinstall",
        ),
        (
            ".npm/_cacache",
            "npm Content-Addressable Cache",
            "Tarball & integrity cache inside ~/.npm/_cacache",
            "Re-fetched automatically by npm install",
        ),
        (
            "Library/pnpm/store",
            "pnpm Global Content Store",
            "Hard-linked package store in ~/Library/pnpm/store",
            "pnpm store prune / re-fetched on pnpm install",
        ),
        (
            "Library/Caches/Yarn",
            "Yarn Package Cache",
            "Cached JavaScript packages in ~/Library/Caches/Yarn",
            "Re-fetched automatically by yarn install",
        ),
        (
            ".bun/install/cache",
            "Bun Global Package Cache",
            "Unpacked registry modules in ~/.bun/install/cache",
            "Re-fetched automatically by bun install",
        ),
        (
            ".cache/uv",
            "uv Python Wheel & Archive Cache",
            "Cached Python wheels and source distributions in ~/.cache/uv",
            "Re-populated automatically by uv sync / uv pip",
        ),
        (
            "Library/Caches/pip",
            "pip HTTP & Wheel Cache",
            "Downloaded Python wheels in ~/Library/Caches/pip",
            "Re-downloaded automatically by pip install",
        ),
        (
            "Library/Developer/Xcode/DerivedData",
            "Xcode DerivedData",
            "Intermediate Swift/Clang module caches, indexes, and build products",
            "Rebuilt automatically by Xcode on next build",
        ),
        (
            "Library/Developer/Xcode/iOS DeviceSupport",
            "Xcode iOS DeviceSupport Symbols",
            "Debug symbol caches copied from connected iPhones/iPads",
            "Re-extracted automatically when device reconnects",
        ),
        (
            "Library/Developer/CoreSimulator/Caches",
            "Xcode CoreSimulator Caches",
            "dyld shared caches generated for Simulator runtimes",
            "Regenerated automatically on Simulator boot",
        ),
        (
            "AppData/Local/npm-cache",
            "npm Cache",
            "Tarball & integrity cache in %LocalAppData%\\npm-cache",
            "Re-fetched automatically by npm install",
        ),
        (
            "AppData/Local/pnpm/store",
            "pnpm Global Content Store",
            "Hard-linked package store in %LocalAppData%\\pnpm\\store",
            "pnpm store prune / re-fetched on pnpm install",
        ),
        (
            "AppData/Local/Yarn/Cache",
            "Yarn Package Cache",
            "Cached JavaScript packages in %LocalAppData%\\Yarn\\Cache",
            "Re-fetched automatically by yarn install",
        ),
        (
            "AppData/Local/pip/Cache",
            "pip HTTP & Wheel Cache",
            "Downloaded Python wheels in %LocalAppData%\\pip\\Cache",
            "Re-downloaded automatically by pip install",
        ),
        (
            "AppData/Local/uv/cache",
            "uv Python Wheel & Archive Cache",
            "Cached Python wheels in %LocalAppData%\\uv\\cache",
            "Re-populated automatically by uv sync / uv pip",
        ),
        (
            "AppData/Local/go-build",
            "Go Build Cache",
            "Compiled package cache in %LocalAppData%\\go-build",
            "Rebuilt automatically by go build",
        ),
        (
            ".gradle/caches",
            "Gradle Build & Artifact Cache",
            "Cached JVM jars and transform outputs in ~/.gradle/caches",
            "Re-downloaded automatically by ./gradlew",
        ),
    ];

    for (rel, title, subtitle, rebuild) in specs {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let path = home.join(rel);
        if !path.exists() {
            continue;
        }
        if !verify_path_constitution(&path, Category::ToolchainHoards, home).is_allowed() {
            continue;
        }
        let sz = measure_path(&path, cancel, scanned_dirs);
        if sz.physical_bytes < 4_000_000 {
            continue;
        }
        let in_use = matching_running_process(&path, active_procs);
        let safety = SafetyTier::SafeRegenerable;
        *seq += 1;
        let item = ScanItem {
            id: format!("live-hoard-{seq}"),
            name: (*title).into(),
            subtitle: (*subtitle).into(),
            path,
            bytes: sz.physical_bytes,
            logical_bytes: sz.logical_bytes,
            category: Category::ToolchainHoards,
            safety,
            age_days: age_days_from_mtime(now_secs, sz.newest_mtime_secs),
            file_count: sz.file_count,
            rebuild_hint: Some((*rebuild).into()),
            selected: default_selected(safety, &in_use),
            in_use_by: in_use,
        };
        if !notify(tx, waker, ScanMessage::Item(item)) {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_library_caches(
    home: &Path,
    now_secs: u64,
    active_procs: &HashSet<String>,
    installed_apps: &HashSet<String>,
    cancel: &AtomicBool,
    scanned_dirs: &mut usize,
    seq: &mut usize,
    tx: &Sender<ScanMessage>,
    waker: &Option<Waker>,
) {
    let cache_roots: Vec<(PathBuf, &str)> = if platform::IS_WINDOWS {
        vec![
            (home.join("AppData/Local/Temp"), "Temporary Files"),
            (home.join("AppData/Local/CrashDumps"), "Crash Dumps"),
        ]
    } else {
        vec![
            (home.join("Library/Caches"), "App Cache"),
            (home.join("Library/Logs"), "Diagnostic Logs"),
        ]
    };

    for (root, kind_label) in cache_roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            let p = entry.path();
            let Some(leaf) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // Homebrew & pip are already surfaced under ToolchainHoards
            if matches!(leaf, "Homebrew" | "pip" | "Yarn") {
                continue;
            }

            // Orphan detection reads macOS bundle ids; it means nothing on Windows.
            let is_orphan = !platform::IS_WINDOWS && is_likely_orphaned_bundle(leaf, installed_apps);
            let category = if is_orphan {
                Category::OrphanedLeftovers
            } else {
                Category::SystemCaches
            };

            if !verify_path_constitution(&p, category, home).is_allowed() {
                continue;
            }

            let sz = measure_path(&p, cancel, scanned_dirs);
            // Only surface meaningful caches (>= 8 MB) so we never clutter the user with tiny KB files
            if sz.physical_bytes < 8_000_000 {
                continue;
            }
            // Windows temp folders are live scratch space: leave anything touched in the last 3 days.
            if platform::IS_WINDOWS && age_days_from_mtime(now_secs, sz.newest_mtime_secs) < 3 {
                continue;
            }

            let in_use = matching_running_process(&p, active_procs);
            let safety = if is_orphan {
                SafetyTier::ReviewRecommended
            } else {
                SafetyTier::SafeRegenerable
            };
            let pretty = humanize_bundle_id(leaf);
            let subtitle = if is_orphan {
                format!("No matching app found in /Applications · {leaf}")
            } else {
                format!("{kind_label} · {leaf}")
            };

            *seq += 1;
            let item = ScanItem {
                id: format!("live-cache-{seq}"),
                name: pretty,
                subtitle,
                path: p,
                bytes: sz.physical_bytes,
                logical_bytes: sz.logical_bytes,
                category,
                safety,
                age_days: age_days_from_mtime(now_secs, sz.newest_mtime_secs),
                file_count: sz.file_count,
                rebuild_hint: Some(if is_orphan {
                    "Leftover cache from uninstalled or renamed app".into()
                } else {
                    "Recreated automatically when the app launches".into()
                }),
                selected: default_selected(safety, &in_use),
                in_use_by: in_use,
            };
            if !notify(tx, waker, ScanMessage::Item(item)) {
                return;
            }
        }
    }
}

fn is_likely_orphaned_bundle(leaf: &str, installed_apps: &HashSet<String>) -> bool {
    let lower = leaf.to_ascii_lowercase();
    if lower.starts_with("com.apple.")
        || lower.starts_with("apple")
        || lower.contains("cloudkit")
        || lower.contains("llvm")
        || lower.contains("clang")
        || lower.contains("rust")
        || lower.contains("go-build")
        || lower.contains("typescript")
        || lower.contains("node")
        || lower.contains("playwright")
        || lower.contains("cypress")
    {
        return false;
    }
    if !lower.contains('.') {
        return false;
    }
    let tokens: Vec<String> = lower
        .split(['.', '-', '_'])
        .filter(|t| {
            t.len() >= 4
                && !matches!(
                    *t,
                    "com" | "org" | "net" | "io" | "app" | "macos" | "desktop" | "client" | "shipit" | "helper"
                )
        })
        .map(str::to_owned)
        .collect();
    if tokens.is_empty() {
        return false;
    }
    !tokens.iter().any(|tok| {
        installed_apps
            .iter()
            .any(|app| app.contains(tok) || tok.contains(app))
    })
}

fn humanize_bundle_id(leaf: &str) -> String {
    if !leaf.contains('.') {
        return leaf.to_owned();
    }
    let parts: Vec<&str> = leaf
        .split('.')
        .filter(|p| !matches!(p.to_ascii_lowercase().as_str(), "com" | "org" | "net" | "io" | "app"))
        .collect();
    if let Some(main) = parts.first() {
        let mut title = (*main).replace(['-', '_'], " ");
        if let Some(first) = title.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        if leaf.ends_with(".ShipIt") {
            format!("{title} Updater Cache")
        } else {
            format!("{title} Cache")
        }
    } else {
        leaf.to_owned()
    }
}

fn scan_dev_workspaces(
    home: &Path,
    now_secs: u64,
    cancel: &AtomicBool,
    scanned_dirs: &mut usize,
    seq: &mut usize,
    tx: &Sender<ScanMessage>,
    waker: &Option<Waker>,
) {
    let candidate_roots = [
        home.join("Desktop"),
        home.join("Documents"),
        home.join("Developer"),
        home.join("Projects"),
        home.join("Code"),
        home.join("workspace"),
        home.join("repos"),
        home.join("src"),
    ];

    let current_exe_dir = std::env::current_dir().ok();
    let mut queue: Vec<(PathBuf, usize)> = candidate_roots
        .into_iter()
        .filter(|p| p.is_dir())
        .map(|p| (p, 0usize))
        .collect();

    while let Some((dir, depth)) = queue.pop() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if depth > 4 {
            continue;
        }
        // Skip Cleanse's own active workspace so we don't sweep our own warm build cache!
        if let Some(cwd) = &current_exe_dir
            && &dir == cwd
        {
            continue;
        }

        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            let p = entry.path();
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if matches!(
                name,
                ".git" | ".hg" | ".svn" | "Library" | "Pictures" | "Movies" | "Music"
            ) {
                continue;
            }

            if let Some((proof, rebuild_cmd)) = verify_build_artifact_sentinel(&p) {
                if !verify_path_constitution(&p, Category::DevArtifacts, home).is_allowed() {
                    continue;
                }
                let sz = measure_path(&p, cancel, scanned_dirs);
                if sz.physical_bytes < 10_000_000 {
                    continue;
                }
                let project_name = dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("project");
                let age_days = estimate_project_dormancy(&dir, now_secs, sz.newest_mtime_secs);
                let safety = SafetyTier::SafeRegenerable;
                // Only auto-select builds dormant >= 7 days so active today's projects stay untouched by default
                let selected = age_days >= 7;

                *seq += 1;
                let item = ScanItem {
                    id: format!("live-dev-{seq}"),
                    name: format!("{project_name} / {name}"),
                    subtitle: format!("{proof} · rebuilds via `{rebuild_cmd}`"),
                    path: p,
                    bytes: sz.physical_bytes,
                    logical_bytes: sz.logical_bytes,
                    category: Category::DevArtifacts,
                    safety,
                    age_days,
                    file_count: sz.file_count,
                    rebuild_hint: Some((*rebuild_cmd).into()),
                    in_use_by: None,
                    selected,
                };
                if !notify(tx, waker, ScanMessage::Item(item)) {
                    return;
                }
                if *scanned_dirs % 64 == 0 {
                    let _ = notify(
                        tx,
                        waker,
                        ScanMessage::Progress {
                            phase: format!("Scanning workspace `{project_name}`…"),
                            scanned_dirs: *scanned_dirs,
                        },
                    );
                }
            } else if !name.starts_with('.') {
                queue.push((p, depth + 1));
            }
        }
    }
}

/// Estimate how many days a project has been dormant by checking sibling manifest/source mtimes.
fn estimate_project_dormancy(project_dir: &Path, now_secs: u64, fallback_mtime: u64) -> u32 {
    let mut newest = fallback_mtime;
    for marker in [
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "pyproject.toml",
        "src",
    ] {
        let p = project_dir.join(marker);
        if let Ok(m) = std::fs::symlink_metadata(&p) {
            let mt = crate::platform::mtime_secs(&m);
            if mt > newest {
                newest = mt;
            }
        }
    }
    age_days_from_mtime(now_secs, newest)
}

#[allow(clippy::too_many_arguments)]
fn scan_downloads_and_trash(
    home: &Path,
    now_secs: u64,
    installed_apps: &HashSet<String>,
    cancel: &AtomicBool,
    scanned_dirs: &mut usize,
    seq: &mut usize,
    tx: &Sender<ScanMessage>,
    waker: &Option<Waker>,
) {
    // 1. ~/Downloads: Installer Ghosts (.dmg, .pkg, .iso, .ipsw) AND Large File Radar (>100 MB)
    let downloads = home.join("Downloads");
    if let Ok(rd) = std::fs::read_dir(&downloads) {
        for entry in rd.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() || !ft.is_file() {
                continue;
            }
            let p = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let phys = crate::platform::phys_bytes(&meta);
            let logi = meta.len();
            let bytes = if phys > 0 { phys } else { logi };
            let age_days = age_days_from_mtime(now_secs, crate::platform::mtime_secs(&meta));
            let Some(file_name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };

            let is_installer = p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| INSTALLER_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));

            if is_installer && bytes >= 5_000_000 {
                if !verify_path_constitution(&p, Category::InstallerGhosts, home).is_allowed() {
                    continue;
                }
                let stem_norm: String = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .chars()
                    .filter(|c| c.is_ascii_alphabetic())
                    .collect::<String>()
                    .to_ascii_lowercase();
                let already_installed = stem_norm.len() >= 4
                    && installed_apps
                        .iter()
                        .any(|app| stem_norm.contains(app) || app.contains(&stem_norm));

                *seq += 1;
                let item = ScanItem {
                    id: format!("live-dmg-{seq}"),
                    name: file_name.to_owned(),
                    subtitle: if already_installed {
                        "Disk image in ~/Downloads · App already installed in /Applications".into()
                    } else {
                        "Standalone installer image in ~/Downloads".into()
                    },
                    path: p,
                    bytes,
                    logical_bytes: logi,
                    category: Category::InstallerGhosts,
                    safety: SafetyTier::ReviewRecommended,
                    age_days,
                    file_count: 1,
                    rebuild_hint: Some("Re-downloadable from vendor website if needed".into()),
                    in_use_by: None,
                    selected: already_installed && age_days >= 14,
                };
                if !notify(tx, waker, ScanMessage::Item(item)) {
                    return;
                }
            } else if bytes >= 100_000_000 {
                // Large File Radar (>100 MB personal/downloaded file — Inspect Only!)
                *seq += 1;
                let sparse_note = if logi > bytes.saturating_mul(2) {
                    format!(
                        "Sparse file: {} apparent, only {} allocated on APFS",
                        crate::model::format_bytes(logi),
                        crate::model::format_bytes(bytes)
                    )
                } else {
                    "Large file in ~/Downloads · Protected from automated sweeps".into()
                };
                let item = ScanItem {
                    id: format!("live-heavy-{seq}"),
                    name: file_name.to_owned(),
                    subtitle: sparse_note,
                    path: p,
                    bytes,
                    logical_bytes: logi,
                    category: Category::Heavyweights,
                    safety: SafetyTier::ProtectedInspectOnly,
                    age_days,
                    file_count: 1,
                    rebuild_hint: Some("Inspect in Finder — never touched by Cleanse".into()),
                    in_use_by: None,
                    selected: false,
                };
                if !notify(tx, waker, ScanMessage::Item(item)) {
                    return;
                }
            }
        }
    }

    // 2. Additional Large File Radar check on ~/Desktop and ~/Movies (top-level files >= 150 MB)
    for folder in ["Desktop", "Movies"] {
        let dir = home.join(folder);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() || !ft.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let phys = crate::platform::phys_bytes(&meta);
            let logi = meta.len();
            let bytes = if phys > 0 { phys } else { logi };
            if bytes < 150_000_000 {
                continue;
            }
            let p = entry.path();
            let Some(file_name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            *seq += 1;
            let item = ScanItem {
                id: format!("live-heavy-{seq}"),
                name: file_name.to_owned(),
                subtitle: format!("Large personal file in ~/{folder} · Inspect Only"),
                path: p,
                bytes,
                logical_bytes: logi,
                category: Category::Heavyweights,
                safety: SafetyTier::ProtectedInspectOnly,
                age_days: age_days_from_mtime(now_secs, crate::platform::mtime_secs(&meta)),
                file_count: 1,
                rebuild_hint: Some("Personal file — locked out of automated cleaning".into()),
                in_use_by: None,
                selected: false,
            };
            if !notify(tx, waker, ScanMessage::Item(item)) {
                return;
            }
        }
    }

    // 3. ~/.Trash (if Full Disk Access allows reading `~/.Trash`)
    let trash = home.join(".Trash");
    if let Ok(rd) = std::fs::read_dir(&trash) {
        for entry in rd.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let p = entry.path();
            if !verify_path_constitution(&p, Category::TrashBin, home).is_allowed() {
                continue;
            }
            let sz = measure_path(&p, cancel, scanned_dirs);
            if sz.physical_bytes < 2_000_000 {
                continue;
            }
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            *seq += 1;
            let item = ScanItem {
                id: format!("live-trash-{seq}"),
                name: name.to_owned(),
                subtitle: "Sitting in ~/.Trash · Still occupying APFS blocks".into(),
                path: p,
                bytes: sz.physical_bytes,
                logical_bytes: sz.logical_bytes,
                category: Category::TrashBin,
                safety: SafetyTier::ReviewRecommended,
                age_days: age_days_from_mtime(now_secs, sz.newest_mtime_secs),
                file_count: sz.file_count,
                rebuild_hint: Some("Already trashed by user".into()),
                in_use_by: None,
                selected: false,
            };
            if !notify(tx, waker, ScanMessage::Item(item)) {
                return;
            }
        }
    }
}
