//! Constitutional 5-Layer Safety Engine (`safety.rs`).
//!
//! Guarantees that Cleanse can NEVER delete or move files necessary for macOS,
//! user credentials, personal documents, git history, or active running applications.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use crate::model::{Category, SafetyTier, ScanItem};
use crate::platform;

/// Reason a path was blocked by the Constitutional Safety Engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SafetyVerdict {
    Allowed { proof: &'static str },
    Denied { reason: String },
}

impl SafetyVerdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, SafetyVerdict::Allowed { .. })
    }
}

/// System-level prefixes that are unconditionally off-limits.
const FORBIDDEN_SYSTEM_PREFIXES: &[&str] = &[
    "/System",
    "/usr",
    "/bin",
    "/sbin",
    "/etc",
    "/var",
    "/private/etc",
    "/private/var/db",
    "/private/var/vm",
    "/Library",
    "/Applications",
    "/Volumes",
    "/dev",
    "/cores",
    "/opt/homebrew/bin",
    "/opt/homebrew/Cellar",
    "/opt/homebrew/opt",
    "/nix",
];

/// Relative paths under `$HOME` that hold irreplaceable identity, credentials, settings, or Apple data vaults.
const FORBIDDEN_HOME_RELATIVES: &[&str] = &[
    "Applications",
    "Pictures",
    "Movies",
    "Music",
    "Public",
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".config",
    ".zshrc",
    ".bashrc",
    ".zprofile",
    ".bash_profile",
    ".gitconfig",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".cargo/bin",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".cargo/config.toml",
    ".rustup/toolchains",
    ".clearance-vault",
    "Library/Keychains",
    "Library/Passwords",
    "Library/Mail",
    "Library/Messages",
    "Library/Photos",
    "Library/Safari",
    "Library/Calendars",
    "Library/Contacts",
    "Library/Notes",
    "Library/Reminders",
    "Library/Mobile Documents",
    "Library/CloudStorage",
    "Library/Group Containers",
    "Library/Preferences",
    "Library/Fonts",
    "Library/LaunchAgents",
    "Library/Services",
    "Library/Accounts",
    "Library/IdentityServices",
    "Library/HomeKit",
    "Library/Health",
    // Windows equivalents: identity, settings, cloud-synced and personal folders.
    "Videos",
    "Contacts",
    "Favorites",
    "Saved Games",
    "OneDrive",
    "NTUSER.DAT",
    "AppData/Roaming",
    "AppData/Local/Microsoft",
    "AppData/Local/Packages",
    "AppData/Local/Programs",
];

/// First folder under a Windows drive root that belongs to the operating system or installed
/// programs. Compared case-insensitively.
const FORBIDDEN_WINDOWS_ROOT_DIRS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "$recycle.bin",
    "system volume information",
    "recovery",
    "boot",
    "perflogs",
    "documents and settings",
    "msocache",
    "$windows.~bt",
    "$windows.~ws",
];

/// `true` if the first component below a Windows drive root is an OS or program-install folder.
pub fn is_windows_system_dir(first_component: &str) -> bool {
    FORBIDDEN_WINDOWS_ROOT_DIRS.iter().any(|d| d.eq_ignore_ascii_case(first_component))
}

/// Directory component names that must never be traversed into or deleted.
const PROTECTED_PATH_COMPONENTS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "Keychains",
    "Mobile Documents",
    "CloudStorage",
];

/// Verified installer/archive extensions eligible for `InstallerGhosts` inside `~/Downloads` only.
pub const INSTALLER_EXTENSIONS: &[&str] = &["dmg", "pkg", "iso", "ipsw", "msi", "msix", "appx"];

/// Normalize a path lexically without following symlinks (prevents symlink escape attacks).
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Check if `path` starts with `prefix` at a clean path-component boundary.
fn has_prefix(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}

/// Verify whether a build artifact directory (`target`, `node_modules`, `.next`, `.venv`, etc.)
/// has a genuine parent project sentinel file proving it is 100% tool-generated.
pub fn verify_build_artifact_sentinel(dir: &Path) -> Option<(&'static str, &'static str)> {
    let name = dir.file_name()?.to_str()?;
    let parent = dir.parent()?;

    match name {
        "target" => {
            // Require Cargo.toml in parent AND at least one Cargo build marker inside `target/`
            // so a user folder casually named `target` is NEVER touched.
            let has_cargo = parent.join("Cargo.toml").is_file();
            let has_marker = dir.join("CACHEDIR.TAG").is_file()
                || dir.join("debug").is_dir()
                || dir.join("release").is_dir()
                || dir.join(".rustc_info.json").is_file();
            if has_cargo && has_marker {
                Some(("Rust Cargo target/ artifact", "cargo build"))
            } else {
                None
            }
        }
        "node_modules" => {
            if parent.join("package.json").is_file() {
                let cmd = if parent.join("pnpm-lock.yaml").is_file() {
                    "pnpm install"
                } else if parent.join("bun.lockb").is_file() || parent.join("bun.lock").is_file() {
                    "bun install"
                } else if parent.join("yarn.lock").is_file() {
                    "yarn install"
                } else {
                    "npm install"
                };
                Some(("Node.js node_modules/ dependencies", cmd))
            } else {
                None
            }
        }
        ".next" => {
            if parent.join("package.json").is_file() {
                Some(("Next.js build cache (.next/)", "npm run build"))
            } else {
                None
            }
        }
        ".turbo" => {
            if parent.join("package.json").is_file() || parent.join("turbo.json").is_file() {
                Some(("Turborepo local cache (.turbo/)", "turbo build"))
            } else {
                None
            }
        }
        ".svelte-kit" | ".nuxt" | ".astro" => {
            if parent.join("package.json").is_file() {
                Some(("Frontend framework build cache", "npm run build"))
            } else {
                None
            }
        }
        ".venv" | "venv" => {
            let has_py = parent.join("pyproject.toml").is_file()
                || parent.join("requirements.txt").is_file();
            let has_cfg = dir.join("pyvenv.cfg").is_file();
            if has_py && has_cfg {
                Some(("Python virtual environment (pyvenv.cfg verified)", "uv sync / pip install -r requirements.txt"))
            } else {
                None
            }
        }
        ".pytest_cache" | "__pycache__" | ".mypy_cache" | ".ruff_cache" => {
            Some(("Python bytecode / linter cache", "Recreated automatically by Python"))
        }
        ".gradle" => {
            if parent.join("build.gradle").is_file() || parent.join("build.gradle.kts").is_file() || parent.join("settings.gradle").is_file() {
                Some(("Gradle project cache", "./gradlew build"))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Run all 5 constitutional barriers against `item` and `home_dir` before any clean/vault action.
pub fn verify_item_for_clean(item: &ScanItem, home_dir: &Path, active_processes: &HashSet<String>) -> SafetyVerdict {
    // Barrier 0: Tier check
    if !item.safety.can_clean() || item.category == Category::Heavyweights {
        return SafetyVerdict::Denied {
            reason: "Item is classified Protected · Inspect Only and cannot be cleaned".into(),
        };
    }

    if let Some(proc_name) = &item.in_use_by {
        return SafetyVerdict::Denied {
            reason: format!("Skipped because '{proc_name}' is currently running"),
        };
    }

    // Check live process match again right before execution
    if let Some(live) = matching_running_process(&item.path, active_processes) {
        return SafetyVerdict::Denied {
            reason: format!("Cache owner '{live}' is currently active"),
        };
    }

    verify_path_constitution(&item.path, item.category, home_dir)
}

/// Pure path-level constitutional check (usable both on live filesystem and in unit tests).
pub fn verify_path_constitution(raw_path: &Path, category: Category, home_dir: &Path) -> SafetyVerdict {
    if !raw_path.is_absolute() {
        return SafetyVerdict::Denied {
            reason: "Relative paths are unconditionally rejected".into(),
        };
    }

    // File names ignore case on Windows, so compare everything lower-cased there.
    let path = platform::fold(lexical_normalize(raw_path));
    let home = platform::fold(lexical_normalize(home_dir));
    let hj = |rel: &str| platform::fold(home.join(rel));

    // Never allow a drive/filesystem root or the home root itself
    if path.parent().is_none() || path == home {
        return SafetyVerdict::Denied {
            reason: "Root and Home directories are constitutionally protected".into(),
        };
    }

    // Barrier 1: Symlink check on existing paths (refuse symlinks as top-level targets)
    if let Ok(meta) = std::fs::symlink_metadata(&path)
        && meta.file_type().is_symlink()
    {
        return SafetyVerdict::Denied {
            reason: "Symlinks are never followed or removed by Cleanse".into(),
        };
    }

    // Barrier 2: Protected component names anywhere in the path (e.g., `.git`)
    for comp in path.components() {
        if let Component::Normal(os) = comp
            && let Some(s) = os.to_str()
            && (PROTECTED_PATH_COMPONENTS.iter().any(|p| p.eq_ignore_ascii_case(s))
                || s.to_ascii_lowercase().starts_with("onedrive"))
        {
            return SafetyVerdict::Denied {
                reason: format!("Path contains protected component '{s}'"),
            };
        }
    }

    // Barrier 3: System prefix deny-list
    for sys in FORBIDDEN_SYSTEM_PREFIXES {
        if has_prefix(&path, Path::new(sys)) {
            return SafetyVerdict::Denied {
                reason: format!("System path under '{sys}' is constitutionally protected"),
            };
        }
    }

    // Windows: the OS, installed programs and recovery data live in fixed top-level folders.
    if let Some(first) = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .next()
        && path.components().any(|c| matches!(c, Component::Prefix(_)))
        && is_windows_system_dir(first)
    {
        return SafetyVerdict::Denied {
            reason: format!("Windows system folder '{first}' is constitutionally protected"),
        };
    }

    // Must reside strictly inside user's `$HOME`
    if !has_prefix(&path, &home) {
        return SafetyVerdict::Denied {
            reason: "Target is outside user's home directory".into(),
        };
    }

    // Barrier 4: Protected `$HOME` subtrees (Keychains, Mail, iCloud, SSH, Preferences, etc.)
    for rel in FORBIDDEN_HOME_RELATIVES {
        let protected = hj(rel);
        if path == protected || has_prefix(&path, &protected) {
            return SafetyVerdict::Denied {
                reason: format!("Protected user data path '~/{rel}' cannot be touched"),
            };
        }
    }

    // Protect direct top-level user folders themselves (`~/Desktop`, `~/Documents`, `~/Downloads`, `~/Library`)
    for top in ["Desktop", "Documents", "Downloads", "Library", "Developer", "Projects", ".Trash", ".cargo", ".cache", "Pictures", "AppData"] {
        if path == hj(top) {
            return SafetyVerdict::Denied {
                reason: format!("Top-level user folder '~/{top}' itself cannot be removed; only verified child items"),
            };
        }
    }

    // Barrier 5: Positive allowlist proof by Category
    match category {
        Category::SystemCaches | Category::OrphanedLeftovers => {
            let caches = hj("Library/Caches");
            let logs = hj("Library/Logs");
            let diag = hj("Library/DiagnosticReports");
            let saved_state = hj("Library/Saved Application State");
            // Windows scratch folders: only their contents, never the folder itself.
            let win_scratch = [hj("AppData/Local/Temp"), hj("AppData/Local/CrashDumps")];
            let in_win_scratch = win_scratch.iter().any(|r| path != *r && has_prefix(&path, r));
            if has_prefix(&path, &caches)
                || in_win_scratch
                || has_prefix(&path, &logs)
                || has_prefix(&path, &diag)
                || has_prefix(&path, &saved_state)
            {
                // Also block cleaning Cleanse's own or critical Apple system daemon caches
                if let Some(leaf) = path.file_name().and_then(|n| n.to_str()) {
                    let lower = leaf.to_ascii_lowercase();
                    if lower.starts_with("com.apple.bird")
                        || lower.starts_with("cloudkit")
                        || lower.starts_with("com.apple.apfs")
                        || lower.contains("keychain")
                    {
                        return SafetyVerdict::Denied {
                            reason: "Core iCloud / APFS system cache is protected".into(),
                        };
                    }
                }
                SafetyVerdict::Allowed {
                    proof: "Verified regenerable ~/Library cache or log directory",
                }
            } else {
                SafetyVerdict::Denied {
                    reason: "Cache item is outside verified ~/Library/{Caches,Logs} roots".into(),
                }
            }
        }
        Category::ToolchainHoards => {
            let allowed_roots = [
                hj(".cargo/registry/cache"),
                hj(".cargo/registry/src"),
                hj(".cargo/git/checkouts"),
                hj("Library/Caches/Homebrew"),
                hj(".npm/_cacache"),
                hj("Library/pnpm/store"),
                hj("Library/Caches/Yarn"),
                hj(".bun/install/cache"),
                hj(".cache/uv"),
                hj(".cache/pip"),
                hj("Library/Caches/pip"),
                hj("Library/Developer/Xcode/DerivedData"),
                hj("Library/Developer/Xcode/iOS DeviceSupport"),
                hj("Library/Developer/CoreSimulator/Caches"),
                hj(".gradle/caches"),
                hj("AppData/Local/npm-cache"),
                hj("AppData/Local/pnpm/store"),
                hj("AppData/Local/Yarn/Cache"),
                hj("AppData/Local/pip/Cache"),
                hj("AppData/Local/uv/cache"),
                hj("AppData/Local/go-build"),
            ];
            if allowed_roots.iter().any(|r| path == *r || has_prefix(&path, r)) {
                SafetyVerdict::Allowed {
                    proof: "Verified SDK or package-manager download cache",
                }
            } else {
                SafetyVerdict::Denied {
                    reason: "Path is not a recognized SDK or package-manager cache".into(),
                }
            }
        }
        Category::DevArtifacts => {
            // If the path currently exists on disk, re-verify its parent sentinel (`Cargo.toml`, `package.json`, etc.)!
            if path.exists() {
                if let Some((proof, _)) = verify_build_artifact_sentinel(&path) {
                    SafetyVerdict::Allowed { proof }
                } else {
                    SafetyVerdict::Denied {
                        reason: "Directory lacks verified project build sentinel (e.g. Cargo.toml / package.json)".into(),
                    }
                }
            } else {
                // Non-existent path (e.g., Dry-Run on Demo/WorstCase fixture): check leaf artifact name
                let valid_leaf = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| {
                        matches!(
                            n,
                            "target"
                                | "node_modules"
                                | ".next"
                                | ".turbo"
                                | ".svelte-kit"
                                | ".nuxt"
                                | ".venv"
                                | "venv"
                                | ".gradle"
                                | "__pycache__"
                                | ".pytest_cache"
                        )
                    });
                if valid_leaf {
                    SafetyVerdict::Allowed {
                        proof: "Verified build artifact leaf signature",
                    }
                } else {
                    SafetyVerdict::Denied {
                        reason: "Unrecognized build artifact directory name".into(),
                    }
                }
            }
        }
        Category::InstallerGhosts => {
            let downloads = hj("Downloads");
            let ext_ok = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| INSTALLER_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
            if has_prefix(&path, &downloads) && ext_ok {
                SafetyVerdict::Allowed {
                    proof: "Verified standalone disk image / installer package in ~/Downloads",
                }
            } else {
                SafetyVerdict::Denied {
                    reason: "Installer Ghosts must be .dmg/.pkg/.iso/.ipsw files strictly inside ~/Downloads".into(),
                }
            }
        }
        Category::TrashBin => {
            let trash = hj(".Trash");
            if has_prefix(&path, &trash) && path != trash {
                SafetyVerdict::Allowed {
                    proof: "Item already placed in ~/.Trash by user",
                }
            } else {
                SafetyVerdict::Denied {
                    reason: "Trash item is not inside ~/.Trash".into(),
                }
            }
        }
        Category::Heavyweights => SafetyVerdict::Denied {
            reason: "Large File Radar items are strictly Inspect-Only".into(),
        },
    }
}

/// Snapshot active process command names on macOS (`ps -axo comm=`) so we never clean a cache
/// belonging to a running app!
pub fn snapshot_running_processes() -> HashSet<String> {
    platform::running_process_names()
}

/// Check if a cache folder name matches a currently running application token.
pub fn matching_running_process(path: &Path, active_processes: &HashSet<String>) -> Option<String> {
    let leaf = path.file_name()?.to_str()?.to_ascii_lowercase();
    // Extract meaningful tokens from reverse-DNS bundle IDs (`com.spotify.client` -> `spotify`)
    let candidates: Vec<&str> = leaf
        .split(['.', '-', '_', ' '])
        .filter(|tok| {
            tok.len() >= 4
                && !matches!(
                    *tok,
                    "com" | "org" | "net" | "io" | "app" | "macos" | "apple" | "client" | "desktop" | "helper" | "caches" | "cache" | "shipit"
                )
        })
        .collect();

    for tok in candidates {
        if active_processes.contains(tok) {
            let mut pretty = tok.to_owned();
            if let Some(first) = pretty.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            return Some(pretty);
        }
    }
    None
}

/// Classify default selection state from safety tier and live-process lock.
pub fn default_selected(tier: SafetyTier, in_use_by: &Option<String>) -> bool {
    tier == SafetyTier::SafeRegenerable && in_use_by.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_system_and_credential_paths_unconditionally() {
        let home = Path::new("/Users/tester");

        let dangerous = [
            ("/System/Library/CoreServices/Finder.app", Category::SystemCaches),
            ("/usr/bin/sudo", Category::SystemCaches),
            ("/Library/Application Support/Apple", Category::SystemCaches),
            ("/Applications/Safari.app", Category::OrphanedLeftovers),
            ("/Users/tester/.ssh/id_ed25519", Category::SystemCaches),
            ("/Users/tester/Library/Keychains/login.keychain-db", Category::SystemCaches),
            ("/Users/tester/Library/Mobile Documents/com~apple~CloudDocs/Tax2026.pdf", Category::InstallerGhosts),
            ("/Users/tester/Library/Preferences/com.apple.Finder.plist", Category::SystemCaches),
            ("/Users/tester/Desktop/project/.git/objects", Category::DevArtifacts),
            ("/Users/tester/Documents/Thesis.docx", Category::InstallerGhosts),
            ("/Users/tester/Downloads/FamilyPhoto.jpg", Category::InstallerGhosts),
            ("/Users/tester", Category::SystemCaches),
            ("/", Category::SystemCaches),
        ];

        for (p, cat) in dangerous {
            let verdict = verify_path_constitution(Path::new(p), cat, home);
            assert!(
                !verdict.is_allowed(),
                "Expected {p} ({cat:?}) to be DENIED, but got {verdict:?}"
            );
        }
    }

    #[test]
    fn allows_verified_caches_and_installers_inside_bounds() {
        let home = Path::new("/Users/tester");

        let safe = [
            ("/Users/tester/Library/Caches/com.tinyspeck.slackmacgap.ShipIt", Category::SystemCaches),
            ("/Users/tester/Library/Logs/DiagnosticReports/crash.ips", Category::SystemCaches),
            ("/Users/tester/.cargo/registry/cache/index.crates.io-123", Category::ToolchainHoards),
            ("/Users/tester/Library/Developer/Xcode/DerivedData/MyApp-abc", Category::ToolchainHoards),
            ("/Users/tester/Downloads/Docker-Desktop-4.38.dmg", Category::InstallerGhosts),
        ];

        for (p, cat) in safe {
            let verdict = verify_path_constitution(Path::new(p), cat, home);
            assert!(
                verdict.is_allowed(),
                "Expected {p} ({cat:?}) to be ALLOWED, got {verdict:?}"
            );
        }
    }

    #[test]
    fn blocks_parent_traversal_escape() {
        let home = Path::new("/Users/tester");
        let sneaky = Path::new("/Users/tester/Library/Caches/../../.ssh/id_rsa");
        let verdict = verify_path_constitution(sneaky, Category::SystemCaches, home);
        assert!(!verdict.is_allowed());
    }

    #[test]
    fn windows_scratch_and_package_caches_are_allowed_but_not_their_roots() {
        let home = Path::new("/Users/tester");
        for (p, cat) in [
            ("/Users/tester/AppData/Local/Temp/old-installer", Category::SystemCaches),
            ("/Users/tester/AppData/Local/CrashDumps/app.dmp", Category::SystemCaches),
            ("/Users/tester/AppData/Local/npm-cache", Category::ToolchainHoards),
            ("/Users/tester/AppData/Local/pip/Cache", Category::ToolchainHoards),
            ("/Users/tester/Downloads/Setup.msi", Category::InstallerGhosts),
        ] {
            assert!(verify_path_constitution(Path::new(p), cat, home).is_allowed(), "{p}");
        }
        for (p, cat) in [
            ("/Users/tester/AppData/Local/Temp", Category::SystemCaches),
            ("/Users/tester/AppData/Roaming/Code/User/settings.json", Category::SystemCaches),
            ("/Users/tester/AppData/Local/Microsoft/Windows/Explorer", Category::SystemCaches),
            ("/Users/tester/OneDrive/proj/node_modules", Category::DevArtifacts),
            ("/Users/tester/OneDrive - Contoso/proj/target", Category::DevArtifacts),
            ("/Users/tester/Videos/holiday.mp4", Category::InstallerGhosts),
            ("/Users/tester/NTUSER.DAT", Category::SystemCaches),
        ] {
            assert!(!verify_path_constitution(Path::new(p), cat, home).is_allowed(), "{p}");
        }
    }

    #[test]
    fn windows_system_folders_are_recognised_in_any_case() {
        for d in ["Windows", "WINDOWS", "Program Files", "program files (x86)", "ProgramData", "$Recycle.Bin", "System Volume Information"] {
            assert!(is_windows_system_dir(d), "{d}");
        }
        for d in ["Users", "Projects", "Games"] {
            assert!(!is_windows_system_dir(d), "{d}");
        }
    }
}
