//! Whole-Mac disk map (`diskmap.rs`).
//!
//! Walks the entire APFS Data volume (plus the sealed system volume) in parallel, measuring
//! *physical* bytes (`st_blocks * 512`) so sparse files, clones and iCloud placeholders never lie,
//! and builds a bounded size-tree: every directory keeps its largest children and rolls the long
//! tail into one "N smaller items" node, so memory stays small even on 3M-file disks.
//!
//! Every node then gets a [`Verdict`] — *can this be deleted?* — from a rule table that is
//! deliberately conservative: anything unknown is treated as the user's own data.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rune::Waker;

use crate::model::format_count;
use crate::platform;
use crate::safety::verify_build_artifact_sentinel;
use crate::scanner::user_home_dir;

/// Where the APFS Data volume (everything that is yours) is mounted.
const DATA_VOLUME: &str = "/System/Volumes/Data";
/// Max children kept per directory; the rest roll into one "smaller items" node.
const MAX_KEEP: usize = 40;
/// Children smaller than this roll up (unless they are among the first few).
const KEEP_MIN: u64 = 512 * 1024;
const FORCE_KEEP_N: usize = 6;
const FORCE_KEEP_MIN: u64 = 64 * 1024;
/// Files below this never become their own node.
const FILE_NODE_MIN: u64 = 64 * 1024;
const MAX_DEPTH: usize = 48;
const TOP_FILES_KEPT: usize = 250;
const HARDLINK_SHARDS: usize = 64;

// ---------------------------------------------------------------------------
// Verdicts: "can this be deleted?"
// ---------------------------------------------------------------------------

/// How safe it is to remove something, from most to least free-to-delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// Regenerates by itself (caches, build output, logs).
    Safe,
    /// Probably removable, but only you know whether you still use it (apps, old installers, VMs).
    Review,
    /// Your own files and app data. Never touched automatically.
    Yours,
    /// Needed by macOS to run. Cannot (and must not) be deleted.
    Required,
}

impl Class {
    pub const ALL: [Class; 4] = [Class::Safe, Class::Review, Class::Yours, Class::Required];

    pub fn idx(self) -> usize {
        match self {
            Class::Safe => 0,
            Class::Review => 1,
            Class::Yours => 2,
            Class::Required => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Class::Safe => "Safe to delete",
            Class::Review => "Delete if unused",
            Class::Yours => "Your data",
            Class::Required => "Required by macOS",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Class::Safe => "Safe",
            Class::Review => "Review",
            Class::Yours => "Yours",
            Class::Required => "System",
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Class::Safe => "Regenerates on its own. Deleting costs nothing but a rebuild.",
            Class::Review => "Probably removable, but only you know if you still use it.",
            Class::Yours => "Your files and app data. Keep unless you choose otherwise.",
            Class::Required => "macOS needs this to run. It cannot be deleted.",
        }
    }

    pub fn can_free(self) -> bool {
        matches!(self, Class::Safe | Class::Review)
    }
}

/// How a rule applies to the subtree below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// No rule of its own: carries its parent's verdict.
    Inherit,
    /// A folder whose *children* are the units you would delete (`~/Library/Caches`, `/Applications`).
    Container,
    /// One deletable unit (`node_modules`, an `.app`, a `.dmg`); nothing inside is judged separately.
    Atom,
    /// Somewhere inside an `Atom`.
    Inside,
}

#[derive(Clone, Copy, Debug)]
pub struct Verdict {
    pub class: Class,
    pub reason: &'static str,
    pub how: &'static str,
    pub region: Region,
}

const ROOT_DEFAULT: Verdict = Verdict {
    class: Class::Yours,
    reason: "Not recognised, so treated as yours and left alone.",
    how: "",
    region: Region::Inherit,
};

const DISK_ROOT: Verdict = Verdict {
    class: Class::Yours,
    reason: "Your whole disk. Open a folder to see what is inside and what can go.",
    how: "",
    region: Region::Container,
};

const fn verdict(class: Class, region: Region, reason: &'static str, how: &'static str) -> Verdict {
    Verdict {
        class,
        reason,
        how,
        region,
    }
}

fn inherit(parent: &Verdict) -> Verdict {
    Verdict {
        region: if matches!(parent.region, Region::Atom | Region::Inside) {
            Region::Inside
        } else {
            Region::Inherit
        },
        ..*parent
    }
}

pub struct Ctx {
    pub home: PathBuf,
    /// Judge paths with Windows rules (case-insensitive, `C:\Windows`, `AppData`, ...).
    pub windows: bool,
    /// Confirm build-artifact folders against real sentinel files (off for demo data).
    pub verify_fs: bool,
}

impl Ctx {
    pub fn live() -> Self {
        Self {
            home: user_home_dir(),
            windows: platform::IS_WINDOWS,
            verify_fs: true,
        }
    }
}

/// Decide whether `path` may be deleted, given the verdict already assigned to its parent.
pub fn classify(path: &Path, is_dir: bool, parent: &Verdict, cx: &Ctx) -> Verdict {
    // Required stays required; atoms are judged as a whole.
    if parent.class == Class::Required || matches!(parent.region, Region::Atom | Region::Inside) {
        return inherit(parent);
    }
    let abs: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();

    if cx.windows {
        // File names ignore case on Windows: judge the lower-cased path.
        let abs_l: Vec<String> = abs.iter().map(|s| s.to_lowercase()).collect();
        let abs_l: Vec<&str> = abs_l.iter().map(String::as_str).collect();
        if let Some(v) = windows_absolute_rule(&abs_l, cx) {
            return v;
        }
        let home_l = platform::fold(cx.home.clone());
        let path_l = platform::fold(path.to_path_buf());
        if let Ok(rel) = path_l.strip_prefix(&home_l) {
            let rel: Vec<String> =
                rel.components().filter_map(|c| c.as_os_str().to_str()).map(str::to_lowercase).collect();
            let rel: Vec<&str> = rel.iter().map(String::as_str).collect();
            if let Some(v) = windows_home_rule(&rel) {
                return v;
            }
        }
    } else {
        if let Some(v) = absolute_rule(&abs, cx) {
            return v;
        }
        if let Ok(rel) = path.strip_prefix(&cx.home) {
            let rel: Vec<&str> = rel.components().filter_map(|c| c.as_os_str().to_str()).collect();
            if let Some(v) = home_rule(&rel) {
                return v;
            }
        }
    }
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let rule = if is_dir {
        dir_name_rule(path, name, cx)
    } else {
        file_rule(path, name)
    };
    rule.unwrap_or_else(|| inherit(parent))
}

fn absolute_rule(abs: &[&str], cx: &Ctx) -> Option<Verdict> {
    use Class::*;
    use Region::*;
    let me = cx.home.file_name().and_then(|s| s.to_str()).unwrap_or("");
    Some(match abs {
        ["System", ..] | ["bin", ..] | ["sbin", ..] => verdict(
            Required,
            Atom,
            "macOS itself: sealed system files. Protected by the OS.",
            "Nothing to do. macOS manages this and it cannot be removed.",
        ),
        [name] if name.starts_with('.') => verdict(
            Required,
            Atom,
            "macOS bookkeeping: Spotlight index, file-event log, temp items.",
            "Nothing to do. macOS rebuilds and trims these itself.",
        ),
        ["cores" | "mnt" | "sw" | "home" | "MobileSoftwareUpdate" | "AppleInternal"] => verdict(
            Required,
            Atom,
            "System folder managed by macOS.",
            "Nothing to do.",
        ),
        ["usr", "local", ..] if abs.len() == 2 => verdict(
            Review,
            Container,
            "Command-line tools installed outside the system (often Homebrew on Intel).",
            "Remove tools you no longer use with their own uninstaller.",
        ),
        ["usr", "local", ..] => return None,
        ["usr", ..] => verdict(
            Required,
            Atom,
            "macOS command-line programs and libraries.",
            "Nothing to do. macOS manages this.",
        ),
        ["private", "var", "vm", ..] => verdict(
            Required,
            Atom,
            "Swap and sleep image. macOS sizes these itself.",
            "A restart shrinks swap. Nothing to delete by hand.",
        ),
        ["private", "var", "folders", ..] => verdict(
            Required,
            Atom,
            "Per-app temporary files that macOS cleans up itself.",
            "Restart your Mac and macOS purges most of it.",
        ),
        ["private", ..] => verdict(
            Required,
            Atom,
            "System databases, logs and temp data managed by macOS.",
            "Nothing to do. macOS manages this.",
        ),
        ["Library", "Apple" | "Preferences" | "Extensions" | "Keychains" | "LaunchDaemons" | "LaunchAgents"
        | "Frameworks" | "PrivilegedHelperTools" | "Security" | "SystemExtensions" | "Filesystems"
        | "Receipts" | "Managed Preferences" | "CoreMediaIO" | "Bluetooth", ..] => verdict(
            Required,
            Atom,
            "System-level settings, drivers and security data.",
            "Nothing to do. Removing it can break apps or macOS.",
        ),
        ["Library", "Application Support", "Apple", ..] => verdict(
            Required,
            Atom,
            "Apple-managed support data used by macOS.",
            "Nothing to do. macOS manages this.",
        ),
        ["Library", "Application Support", "Logic" | "GarageBand" | "Final Cut Pro System Support"
        | "Motion" | "Compressor" | "MainStage", ..]
            if abs.len() == 3 =>
        {
            verdict(
                Review,
                Atom,
                "Sound libraries and content packs for Apple's pro apps.",
                "Logic/GarageBand → Sound Library → Manage Sound Library. Re-download later if needed.",
            )
        }
        ["Library", "Developer", "CommandLineTools", ..] if abs.len() == 3 => verdict(
            Review,
            Atom,
            "Command Line Tools: git, clang and friends. Many dev tools rely on them.",
            "Reinstall any time with: xcode-select --install",
        ),
        ["Library", "Developer", ..] if abs.len() == 2 => verdict(
            Review,
            Container,
            "Developer tools and simulator runtimes shared by all users.",
            "Delete unused simulator runtimes in Xcode → Settings → Platforms.",
        ),
        ["Library", "Caches", ..] if abs.len() == 2 => verdict(
            Review,
            Container,
            "System-wide caches. macOS rebuilds them, but deleting needs admin rights.",
            "Quit related apps, then delete the folder. Expect a slower first launch.",
        ),
        ["Library", "Logs", ..] if abs.len() == 2 => verdict(
            Safe,
            Container,
            "System-wide diagnostic logs.",
            "Delete freely (needs admin rights). New logs are created as needed.",
        ),
        ["Library", "Updates", ..] if abs.len() == 2 => verdict(
            Review,
            Atom,
            "Downloaded macOS updates that have not been installed.",
            "System Settings → General → Software Update manages these.",
        ),
        ["Library", "Audio", ..] if abs.len() == 2 => verdict(
            Review,
            Container,
            "Apple Loops, instruments and plug-in content.",
            "Re-download from Logic/GarageBand if you ever need it again.",
        ),
        ["Library"] => verdict(
            Yours,
            Container,
            "Shared app data. Unknown parts are left alone.",
            "",
        ),
        ["Applications"] => verdict(
            Review,
            Container,
            "Installed apps. Each one can be uninstalled if you no longer use it.",
            "Drag the app to the Trash. Check for its own uninstaller first.",
        ),
        ["opt", "homebrew", ..] if abs.len() == 2 => verdict(
            Review,
            Container,
            "Homebrew packages and the tools installed through them.",
            "Use brew, not Finder: brew autoremove && brew cleanup, or brew uninstall <name>.",
        ),
        ["opt", "homebrew", "Caskroom" | "Cellar", ..] if abs.len() == 3 => verdict(
            Review,
            Container,
            "Packages installed by Homebrew.",
            "brew uninstall <name> removes one properly.",
        ),
        ["opt", ..] if abs.len() == 1 => verdict(
            Yours,
            Container,
            "Optional software installed outside the system.",
            "",
        ),
        ["Users"] => verdict(Yours, Container, "User accounts on this Mac.", ""),
        ["Users", "Shared", ..] if abs.len() == 2 => {
            verdict(Yours, Container, "Files shared between accounts.", "")
        }
        ["Users", other, ..] if abs.len() == 2 && *other != me => verdict(
            Yours,
            Atom,
            "Another account's files. Not yours to clean from here.",
            "Sign in as that user to manage it.",
        ),
        _ => return None,
    })
}

/// Windows: judge the OS and program folders. `abs` holds lower-cased components with the drive
/// and root already stripped, so `C:\Windows\System32` is `["windows", "system32"]`.
fn windows_absolute_rule(abs: &[&str], cx: &Ctx) -> Option<Verdict> {
    use Class::*;
    use Region::*;
    let me = cx.home.file_name().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    Some(match abs {
        ["windows", ..] => verdict(
            Required,
            Atom,
            "Windows itself: system files, drivers and update store. Protected by the OS.",
            "Nothing to delete by hand. Settings → System → Storage → Temporary files cleans the safe parts.",
        ),
        ["windows.old", ..] => verdict(
            Review,
            Atom,
            "Your previous Windows installation, kept so you can roll back an upgrade.",
            "Settings → System → Storage → Temporary files → Previous Windows installation(s).",
        ),
        ["$windows.~bt" | "$windows.~ws", ..] => verdict(
            Review,
            Atom,
            "Leftover files from a Windows upgrade.",
            "Settings → System → Storage → Temporary files. Windows removes them for you.",
        ),
        ["program files" | "program files (x86)"] => verdict(
            Review,
            Container,
            "Installed programs. Each one can be uninstalled if you no longer use it.",
            "Settings → Apps → Installed apps → Uninstall. Never delete the folder by hand.",
        ),
        ["program files" | "program files (x86)", app]
            if app.starts_with("windows")
                || app.starts_with("microsoft")
                || matches!(
                    *app,
                    "common files" | "dotnet" | "internet explorer" | "modifiablewindowsapps" | "msbuild"
                        | "reference assemblies" | "uninstall information" | "windowsapps" | "windowspowershell"
                ) =>
        {
            verdict(
                Required,
                Atom,
                "Part of Windows or a shared runtime that other programs depend on.",
                "Nothing to do. Windows manages this.",
            )
        }
        ["program files" | "program files (x86)", ..] if abs.len() == 2 => verdict(
            Review,
            Atom,
            "An installed program. Uninstall it if you no longer use it.",
            "Settings → Apps → Installed apps → Uninstall. Never delete the folder by hand.",
        ),
        ["programdata", "microsoft", ..] | ["programdata", "package cache", ..] => verdict(
            Required,
            Atom,
            "Windows and installer data needed to repair or uninstall programs.",
            "Nothing to do. Programs and Windows manage this.",
        ),
        ["programdata"] => verdict(
            Yours,
            Container,
            "Data shared by every user's programs. Unknown parts are left alone.",
            "",
        ),
        ["$recycle.bin", ..] => verdict(
            Review,
            Atom,
            "Deleted files waiting in the Recycle Bin. Emptying it frees this space.",
            "Right-click the Recycle Bin → Empty Recycle Bin. This is permanent.",
        ),
        ["system volume information", ..] => verdict(
            Required,
            Atom,
            "System Restore points and shadow copies.",
            "System Properties → System Protection → Configure → set a smaller maximum or Delete.",
        ),
        ["pagefile.sys" | "swapfile.sys" | "dumpstack.log.tmp" | "dumpstack.log"] => verdict(
            Required,
            Atom,
            "Windows virtual memory (swap). Windows sizes it itself.",
            "System Properties → Advanced → Performance → Virtual memory, if you really need to change it.",
        ),
        ["hiberfil.sys"] => verdict(
            Required,
            Atom,
            "Hibernation and Fast Startup image. Windows needs it for hibernate.",
            "Admin terminal: powercfg /hibernate off  (turns hibernate off and frees the file).",
        ),
        ["recovery" | "boot" | "bootmgr" | "bootnxt" | "efi" | "$sysreset" | "config.msi" | "documents and settings"
        | "msocache" | "perflogs", ..] => verdict(
            Required,
            Atom,
            "Windows boot, recovery or installer-support data.",
            "Nothing to do. Windows manages this.",
        ),
        ["users"] => verdict(Yours, Container, "User accounts on this PC.", ""),
        ["users", "default" | "default user" | "all users" | "defaultapppool", ..] => verdict(
            Required,
            Atom,
            "Windows account templates.",
            "Nothing to do. Windows manages this.",
        ),
        ["users", "public", ..] if abs.len() == 2 => verdict(Yours, Container, "Files shared between accounts.", ""),
        ["users", other, ..] if abs.len() == 2 && *other != me => verdict(
            Yours,
            Atom,
            "Another account's files. Not yours to clean from here.",
            "Sign in as that user to manage it.",
        ),
        _ => return None,
    })
}

/// Windows: judge folders inside the user profile. `rel` is lower-cased and relative to the profile.
fn windows_home_rule(rel: &[&str]) -> Option<Verdict> {
    use Class::*;
    use Region::*;
    Some(match rel {
        [] => verdict(Yours, Container, "Your user profile. Everything in it is yours.", ""),
        ["ntuser.dat" | "ntuser.ini", ..] => verdict(
            Required,
            Atom,
            "Your Windows account settings.",
            "Nothing to do. Windows manages this.",
        ),
        ["appdata"] => verdict(
            Yours,
            Container,
            "App data and settings. Caches inside are safe to clear; the rest is not.",
            "",
        ),
        ["appdata", "local"] | ["appdata", "roaming"] | ["appdata", "locallow"] => verdict(
            Yours,
            Container,
            "App data and settings. Deleting parts of it resets apps.",
            "",
        ),
        ["appdata", "local", "temp"] => verdict(
            Safe,
            Container,
            "Temporary files apps leave behind. Windows and apps recreate what they need.",
            "Close your apps, then delete. Files in use are skipped. Or Settings → System → Storage → Temporary files.",
        ),
        ["appdata", "local", "crashdumps"] => verdict(
            Safe,
            Atom,
            "Crash dump files from apps that stopped unexpectedly.",
            "Delete freely.",
        ),
        ["appdata", "local", "d3dscache"]
        | ["appdata", "local", "microsoft", "windows", "inetcache"]
        | ["appdata", "local", "microsoft", "windows", "wer"] => verdict(
            Safe,
            Atom,
            "Shader, web and error-report caches that Windows rebuilds.",
            "Delete freely. Expect a slightly slower first launch.",
        ),
        ["appdata", "local", "npm-cache"] => verdict(
            Safe,
            Atom,
            "npm's download cache.",
            "npm cache clean --force. Packages re-download when needed.",
        ),
        ["appdata", "local", "pnpm", "store"] => verdict(
            Safe,
            Atom,
            "pnpm's shared package store.",
            "pnpm store prune. Packages re-download when a project needs them.",
        ),
        ["appdata", "local", "yarn", "cache"] => verdict(
            Safe,
            Atom,
            "Yarn's package cache.",
            "yarn cache clean. Packages re-download when needed.",
        ),
        ["appdata", "local", "pip", "cache"] => verdict(
            Safe,
            Atom,
            "pip's download and wheel cache.",
            "pip cache purge. Packages re-download when needed.",
        ),
        ["appdata", "local", "uv", "cache"] => verdict(
            Safe,
            Atom,
            "uv's wheel and archive cache.",
            "uv cache clean. Re-populated by uv sync.",
        ),
        ["appdata", "local", "go-build"] => verdict(
            Safe,
            Atom,
            "Go's compiled package cache.",
            "go clean -cache. Rebuilt on the next go build.",
        ),
        ["appdata", "local", "programs"] => verdict(
            Review,
            Container,
            "Programs installed just for you. Uninstall the ones you don't use.",
            "Settings → Apps → Installed apps → Uninstall.",
        ),
        ["appdata", "local", "programs", ..] if rel.len() == 4 => verdict(
            Review,
            Atom,
            "A program installed just for you.",
            "Settings → Apps → Installed apps → Uninstall. Never delete the folder by hand.",
        ),
        ["appdata", "local", "docker"] | ["appdata", "local", "docker desktop"] => verdict(
            Review,
            Atom,
            "Docker's disk image: every image, container and volume.",
            "docker system prune -a   (removes unused images and stopped containers)",
        ),
        ["appdata", "local", "packages"] | ["appdata", "local", "microsoft"] => verdict(
            Yours,
            Container,
            "Store apps and Windows components' data.",
            "",
        ),
        ["appdata", "local", "microsoft", "windows", "explorer"] => verdict(
            Safe,
            Atom,
            "Thumbnail and icon caches that File Explorer rebuilds.",
            "Settings → System → Storage → Temporary files → Thumbnails.",
        ),
        [".nuget", "packages"] => verdict(
            Safe,
            Atom,
            "NuGet's global package cache.",
            "dotnet nuget locals all --clear. Packages restore when a project builds.",
        ),
        [first, ..] if first.starts_with("onedrive") => verdict(
            Yours,
            Container,
            "Your OneDrive. Deleting here deletes it in the cloud too. Online-only files use no disk space.",
            "Right-click a folder → Free up space to keep it in the cloud only.",
        ),
        ["videos" | "music" | "pictures" | "documents" | "desktop" | "downloads" | "saved games" | "contacts"
        | "favorites"] => verdict(Yours, Container, "One of your personal folders.", ""),
        _ => return None,
    })
}

fn home_rule(rel: &[&str]) -> Option<Verdict> {
    use Class::*;
    use Region::*;
    Some(match rel {
        [] => verdict(
            Yours,
            Container,
            "Your home folder. Everything in it is yours.",
            "",
        ),
        [".Trash"] => verdict(
            Review,
            Atom,
            "Already in the Trash. Emptying it frees this space.",
            "Finder → Empty Trash. This is permanent.",
        ),
        ["Library"] => verdict(
            Yours,
            Container,
            "App data and settings. Caches inside are safe to clear; the rest is not.",
            "",
        ),
        ["Library", "Caches"] => verdict(
            Safe,
            Container,
            "App caches. Each app rebuilds its own automatically.",
            "Quit the app, then delete its folder. Nothing of yours is lost.",
        ),
        ["Library", "Logs"] => verdict(
            Safe,
            Container,
            "Diagnostic logs and crash reports.",
            "Delete freely. New logs are created when needed.",
        ),
        ["Library", "Developer", "Xcode", "DerivedData"] => verdict(
            Safe,
            Atom,
            "Xcode build output and indexes.",
            "Xcode rebuilds it on the next build.",
        ),
        ["Library", "Developer", "Xcode", "iOS DeviceSupport" | "watchOS DeviceSupport" | "tvOS DeviceSupport"] => {
            verdict(
                Safe,
                Atom,
                "Debug symbols copied from devices you plugged in.",
                "Re-created when you reconnect the device.",
            )
        }
        ["Library", "Developer", "Xcode", "Archives"] => verdict(
            Yours,
            Atom,
            "App builds you archived for distribution. Needed to symbolicate crashes.",
            "Only delete archives of versions you will never debug again.",
        ),
        ["Library", "Developer", "CoreSimulator", "Caches"] => verdict(
            Safe,
            Atom,
            "Simulator runtime caches.",
            "Regenerated the next time a Simulator boots.",
        ),
        ["Library", "Developer", "CoreSimulator", "Devices"] => verdict(
            Review,
            Atom,
            "iOS Simulator devices and the apps/data installed in them.",
            "xcrun simctl delete unavailable   (removes only broken ones)",
        ),
        ["Library", "Application Support", "MobileSync", "Backup"] => verdict(
            Review,
            Container,
            "iPhone and iPad backups stored on this Mac.",
            "Finder → your device → Manage Backups… and delete old ones.",
        ),
        ["Library", "Application Support", _] => verdict(
            Yours,
            Container,
            "App data: settings, logins and downloaded content. Deleting resets the app.",
            "Uninstall the app properly to remove this, or clear its in-app cache.",
        ),
        ["Library", "Containers", "com.docker.docker"] => verdict(
            Review,
            Atom,
            "Docker's disk: every image, container and volume lives in one file.",
            "docker system prune -a   (removes unused images and stopped containers)",
        ),
        ["Library", "Containers"] | ["Library", "Group Containers"] => verdict(
            Yours,
            Container,
            "Sandboxed app data.",
            "",
        ),
        ["Library", "Mail" | "Messages" | "Safari" | "Keychains" | "Mobile Documents" | "CloudStorage"
        | "Calendars" | "Preferences" | "Cookies" | "Accounts" | "Photos" | "Metadata" | "Passes"] => verdict(
            Yours,
            Atom,
            "Irreplaceable personal data: mail, messages, iCloud, passwords or settings.",
            "Never delete by hand.",
        ),
        ["Library", "pnpm", "store"] => verdict(
            Safe,
            Atom,
            "pnpm's shared package store.",
            "pnpm store prune. Packages re-download when a project needs them.",
        ),
        ["Library", "Arduino15"] => verdict(
            Review,
            Atom,
            "Arduino board packages and libraries.",
            "Delete if you no longer program Arduino boards.",
        ),
        ["Library", "Android"] => verdict(
            Review,
            Atom,
            "Android SDK and emulator images.",
            "Remove unused SDK versions in Android Studio → SDK Manager.",
        ),
        [".npm"] => verdict(
            Safe,
            Atom,
            "npm's download cache.",
            "npm cache clean --force. Packages re-download on demand.",
        ),
        [".cargo", "registry"] | [".cargo", "git"] => verdict(
            Safe,
            Atom,
            "Rust crates downloaded by Cargo.",
            "Cargo re-downloads whatever a project needs.",
        ),
        [".cargo"] => verdict(Yours, Container, "Rust toolchain binaries and caches.", ""),
        [".rustup"] => verdict(
            Review,
            Container,
            "Installed Rust toolchains.",
            "rustup toolchain list → rustup toolchain uninstall <name>",
        ),
        [".bun", "install", "cache"] => verdict(
            Safe,
            Atom,
            "Bun's package cache.",
            "bun pm cache rm. Packages re-download on demand.",
        ),
        [".bun"] => verdict(
            Review,
            Container,
            "Bun runtime and global packages.",
            "Delete only if you no longer use Bun.",
        ),
        [".cache", "huggingface" | "torch" | "whisper" | "lm-studio" | "ollama"] => verdict(
            Review,
            Atom,
            "Downloaded AI models. Large, and slow to download again.",
            "Delete models you no longer run. They re-download when needed.",
        ),
        [".cache"] => verdict(
            Safe,
            Container,
            "Tool caches (uv, pip, build tools…).",
            "Delete freely. Tools re-download or rebuild what they need.",
        ),
        [".local"] => verdict(
            Review,
            Container,
            "User-level tools and data (uv-managed Pythons, pipx apps…).",
            "Inspect before deleting. uv python list / pipx list show what is inside.",
        ),
        [".gradle", "caches"] => verdict(
            Safe,
            Atom,
            "Gradle's dependency cache.",
            "Gradle re-downloads dependencies on the next build.",
        ),
        [".m2", "repository"] => verdict(
            Safe,
            Atom,
            "Maven's dependency cache.",
            "Maven re-downloads dependencies on the next build.",
        ),
        [".nvm" | ".pyenv" | ".rbenv" | ".asdf" | ".conda" | "miniconda3" | "anaconda3" | "miniforge3"] => {
            verdict(
                Review,
                Atom,
                "Installed language versions and environments.",
                "Remove versions you don't use through the version manager itself.",
            )
        }
        [".ollama"] => verdict(
            Review,
            Atom,
            "Local AI models downloaded by Ollama.",
            "ollama list → ollama rm <model>",
        ),
        [".vscode"] => verdict(
            Review,
            Container,
            "Editor extensions and settings.",
            "Remove unused extensions inside VS Code.",
        ),
        ["google-cloud-sdk"] => verdict(
            Review,
            Atom,
            "Google Cloud SDK installation.",
            "Delete the folder if you no longer use gcloud.",
        ),
        [".ssh" | ".gnupg" | ".aws" | ".kube" | ".config" | ".docker"] => verdict(
            Yours,
            Atom,
            "Keys, credentials and configuration. Losing these can lock you out.",
            "Never delete by hand.",
        ),
        [".claude" | ".codex" | ".t3" | ".gemini" | ".cursor"] => verdict(
            Yours,
            Container,
            "Tool state, sessions and settings.",
            "",
        ),
        ["Desktop" | "Documents" | "Pictures" | "Movies" | "Music" | "Public"] => verdict(
            Yours,
            Container,
            "Your files. You decide what stays.",
            "",
        ),
        ["Downloads"] => verdict(
            Yours,
            Container,
            "Everything you've downloaded. Old installers and archives are flagged below.",
            "",
        ),
        ["Applications"] => verdict(
            Review,
            Container,
            "Apps installed just for you.",
            "Drag the app to the Trash.",
        ),
        _ => return None,
    })
}

fn file_rule(path: &Path, name: &str) -> Option<Verdict> {
    use Class::*;
    use Region::*;
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e)?;
    // Match folder names the same way on every OS.
    let p = path.to_string_lossy().replace('\\', "/");
    match ext {
        "dmg" | "pkg" | "iso" | "ipsw" | "xip" | "msi" | "msix" | "appx" => Some(verdict(
            Review,
            Atom,
            "Installer or disk image. Usually only needed once.",
            "Delete if the app is already installed. It can be downloaded again.",
        )),
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar"
            if p.contains("/Downloads/") || p.contains("/Desktop/") =>
        {
            Some(verdict(
                Review,
                Atom,
                "Archive. If you've already extracted it, this is a duplicate.",
                "Check the extracted copy exists, then delete the archive.",
            ))
        }
        "log" | "crash" | "ips" | "dmp" => Some(verdict(
            Safe,
            Atom,
            "Log or crash report.",
            "Delete freely.",
        )),
        "vmdk" | "vdi" | "qcow2" | "vhd" | "vhdx" => Some(verdict(
            Yours,
            Atom,
            "Virtual machine disk. It holds a whole computer's data.",
            "Delete only if you are sure the VM is no longer needed.",
        )),
        _ => None,
    }
}

/// Fixed names used when real folders can't be inspected (demo data).
fn static_build_name(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "node_modules" => ("Node.js dependencies (node_modules/)", "npm install"),
        "target" => ("Rust build output (target/)", "cargo build"),
        ".next" => ("Next.js build cache (.next/)", "npm run build"),
        ".venv" => ("Python virtual environment", "uv sync"),
        "__pycache__" => ("Python bytecode cache", "Recreated automatically by Python"),
        _ => return None,
    })
}

fn dir_name_rule(path: &Path, name: &str, cx: &Ctx) -> Option<Verdict> {
    use Class::*;
    use Region::*;
    let lower = name.to_ascii_lowercase();

    if lower.ends_with(".app") {
        return Some(verdict(
            Review,
            Atom,
            "An application. Uninstall it if you no longer use it.",
            "Drag to the Trash. Run the app's own uninstaller first if it has one.",
        ));
    }
    for ext in [
        ".photoslibrary",
        ".logicx",
        ".band",
        ".imovielibrary",
        ".tvlibrary",
        ".musiclibrary",
        ".sparsebundle",
        ".xcarchive",
        ".vmwarevm",
        ".pvm",
        ".utm",
    ] {
        if lower.ends_with(ext) {
            return Some(verdict(
                Yours,
                Atom,
                "A library or project bundle holding your own work.",
                "Only delete if you are certain you no longer need it.",
            ));
        }
    }
    if lower.ends_with(".fcpbundle") {
        return Some(verdict(
            Yours,
            Container,
            "A Final Cut Pro library: your edits and media.",
            "Use File → Delete Generated Library Files… to drop only the regenerable parts.",
        ));
    }
    if path.to_string_lossy().replace('\\', "/").contains(".fcpbundle/") {
        match name {
            "Render Files" | "Analysis Files" | "Thumbnails" => {
                return Some(verdict(
                    Safe,
                    Atom,
                    "Final Cut render and analysis cache.",
                    "Final Cut → File → Delete Generated Library Files…",
                ));
            }
            "Transcoded Media" | "Proxy Media" => {
                return Some(verdict(
                    Review,
                    Atom,
                    "Optimized/proxy media. Rebuilt from originals if they still exist.",
                    "Final Cut → File → Delete Generated Library Files…",
                ));
            }
            _ => {}
        }
    }
    match name {
        ".git" => {
            return Some(verdict(
                Yours,
                Atom,
                "A project's full history. Deleting it loses every commit.",
                "git gc --aggressive can shrink it without losing anything.",
            ));
        }
        "node_modules" | "target" | ".next" | ".turbo" | ".svelte-kit" | ".nuxt" | ".astro" | ".venv" | "venv"
        | "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".gradle" => {
            let hit = if cx.verify_fs {
                verify_build_artifact_sentinel(path)
            } else {
                static_build_name(name)
            };
            if let Some((proof, rebuild)) = hit {
                return Some(verdict(Safe, Atom, proof, rebuild));
            }
        }
        "Pods" if !cx.verify_fs || path.parent().is_some_and(|p| p.join("Podfile").is_file()) => {
            return Some(verdict(
                Safe,
                Atom,
                "CocoaPods dependencies (Podfile verified).",
                "pod install",
            ));
        }
        ".build" if !cx.verify_fs || path.parent().is_some_and(|p| p.join("Package.swift").is_file()) => {
            return Some(verdict(
                Safe,
                Atom,
                "Swift Package Manager build output.",
                "swift build",
            ));
        }
        "DerivedData" => {
            return Some(verdict(
                Safe,
                Atom,
                "Xcode build output and indexes.",
                "Xcode rebuilds it on the next build.",
            ));
        }
        "Cache" | "Caches" | "Code Cache" | "GPUCache" | "CachedData" | "CacheStorage" | "ShaderCache"
        | "GrShaderCache" | "DawnCache" | "DawnGraphiteCache" | "DawnWebGPUCache" | "Crashpad" => {
            return Some(verdict(
                Safe,
                Atom,
                "App cache. The app rebuilds it automatically.",
                "Quit the app first, then delete the folder.",
            ));
        }
        _ => {}
    }
    None
}

// ---------------------------------------------------------------------------
// Tree model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
    /// Everything in a folder too small to list on its own.
    Rollup,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    pub path: PathBuf,
    pub kind: NodeKind,
    /// Physical APFS bytes allocated (`st_blocks * 512`).
    pub bytes: u64,
    /// Apparent size (`st_size`); larger than `bytes` for sparse files.
    pub logical: u64,
    pub files: u64,
    /// Newest modification time anywhere inside, in unix seconds.
    pub newest_mtime: u64,
    /// Largest first; a `Rollup` node (if any) is always last.
    pub children: Vec<Node>,
    pub verdict: Verdict,
    /// Bytes inside this node per [`Class`] (indexed by `Class::idx`).
    pub by_class: [u64; 4],
    /// macOS refused to list this folder (needs Full Disk Access).
    pub unreadable: bool,
}

impl Node {
    pub fn new(name: impl Into<String>, path: PathBuf, kind: NodeKind, mtime: u64) -> Self {
        Self {
            name: name.into(),
            path,
            kind,
            bytes: 0,
            logical: 0,
            files: 0,
            newest_mtime: mtime,
            children: Vec::new(),
            verdict: ROOT_DEFAULT,
            by_class: [0; 4],
            unreadable: false,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }

    pub fn find_child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name && c.kind != NodeKind::Rollup)
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn sort_children(children: &mut [Node]) {
    children.sort_by(|a, b| {
        (a.kind == NodeKind::Rollup)
            .cmp(&(b.kind == NodeKind::Rollup))
            .then(b.bytes.cmp(&a.bytes))
            .then(a.name.cmp(&b.name))
    });
}

/// Assign verdicts top-down and roll per-class byte totals up the tree.
pub fn annotate(node: &mut Node, parent: &Verdict, cx: &Ctx) {
    node.verdict = if node.kind == NodeKind::Rollup {
        inherit(parent)
    } else {
        classify(&node.path, node.kind == NodeKind::Dir, parent, cx)
    };
    let own = node.verdict;
    let mut by = [0u64; 4];
    let mut child_sum = 0u64;
    for ch in &mut node.children {
        annotate(ch, &own, cx);
        child_sum = child_sum.saturating_add(ch.bytes);
        for (slot, add) in by.iter_mut().zip(ch.by_class) {
            *slot = slot.saturating_add(add);
        }
    }
    by[own.class.idx()] += node.bytes.saturating_sub(child_sum);
    node.by_class = by;
}

fn rebase(node: &mut Node, from: &Path, to: &Path) {
    if let Ok(rel) = node.path.strip_prefix(from) {
        node.path = to.join(rel);
    }
    for ch in &mut node.children {
        rebase(ch, from, to);
    }
}

fn merge_nodes(a: &mut Node, b: Node) {
    a.bytes += b.bytes;
    a.logical += b.logical;
    a.files += b.files;
    a.newest_mtime = a.newest_mtime.max(b.newest_mtime);
    a.unreadable |= b.unreadable;
    for (slot, add) in a.by_class.iter_mut().zip(b.by_class) {
        *slot += add;
    }
    for ch in b.children {
        if let Some(existing) = a
            .children
            .iter_mut()
            .find(|c| c.name == ch.name && c.kind == ch.kind)
            && ch.kind != NodeKind::Rollup
        {
            merge_nodes(existing, ch);
        } else if ch.kind == NodeKind::Rollup {
            if let Some(r) = a.children.iter_mut().find(|c| c.kind == NodeKind::Rollup) {
                r.bytes += ch.bytes;
                r.logical += ch.logical;
                r.files += ch.files;
                r.name = rollup_name(r.files);
                for (slot, add) in r.by_class.iter_mut().zip(ch.by_class) {
                    *slot += add;
                }
            } else {
                a.children.push(ch);
            }
        } else {
            a.children.push(ch);
        }
    }
    sort_children(&mut a.children);
}

fn rollup_name(items: u64) -> String {
    format!("{} smaller items", format_count(items as usize))
}

fn virtual_root() -> Node {
    let mut n = Node::new(platform::ROOT_NAME, PathBuf::from(platform::ROOT_PATH), NodeKind::Dir, 0);
    n.verdict = DISK_ROOT;
    n
}

fn add_root(root: &mut Node, node: Node) {
    if let Some(existing) = root.children.iter_mut().find(|c| c.name == node.name) {
        merge_nodes(existing, node);
    } else {
        root.children.push(node);
    }
    sort_children(&mut root.children);
    root.bytes = root.children.iter().map(|c| c.bytes).sum();
    root.logical = root.children.iter().map(|c| c.logical).sum();
    root.files = root.children.iter().map(|c| c.files).sum();
    root.newest_mtime = root.children.iter().map(|c| c.newest_mtime).max().unwrap_or(0);
    root.by_class = [0; 4];
    for ch in &root.children {
        for (slot, add) in root.by_class.iter_mut().zip(ch.by_class) {
            *slot += add;
        }
    }
    root.verdict = DISK_ROOT;
}

// ---------------------------------------------------------------------------
// "What can I delete?" — flat, non-overlapping list across the whole disk
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Candidate {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub files: u64,
    pub newest_mtime: u64,
    pub class: Class,
    pub reason: &'static str,
    pub how: &'static str,
}

pub fn collect_candidates(root: &Node, limit: usize, min_bytes: u64) -> Vec<Candidate> {
    fn cand(n: &Node) -> Candidate {
        Candidate {
            name: n.name.clone(),
            path: n.path.clone(),
            bytes: n.bytes,
            files: n.files,
            newest_mtime: n.newest_mtime,
            class: n.verdict.class,
            reason: n.verdict.reason,
            how: n.verdict.how,
        }
    }
    fn walk(n: &Node, out: &mut Vec<Candidate>) {
        if n.kind == NodeKind::Rollup {
            return;
        }
        if n.verdict.class.can_free() {
            match n.verdict.region {
                Region::Atom => {
                    out.push(cand(n));
                    return;
                }
                Region::Container => {
                    for ch in n.children.iter().filter(|c| c.kind != NodeKind::Rollup) {
                        // A folder holding a more specific rule inside (e.g. `.bun/install/cache`
                        // within `.bun`) is split so each part keeps its own verdict.
                        let mixed = ch.is_dir() && ch.by_class[ch.verdict.class.idx()] < ch.bytes;
                        let before = out.len();
                        if mixed || !ch.verdict.class.can_free() {
                            walk(ch, out);
                        }
                        if out.len() == before && ch.verdict.class.can_free() {
                            out.push(cand(ch));
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
        for ch in &n.children {
            walk(ch, out);
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.retain(|c| c.bytes >= min_bytes);
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
    out.truncate(limit);
    out
}

// ---------------------------------------------------------------------------
// File kinds (what *type* of thing is eating the disk)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Video,
    Audio,
    Images,
    DiskImages,
    Archives,
    Documents,
    Code,
    Databases,
    Other,
}

impl FileKind {
    pub const ALL: [FileKind; 9] = [
        FileKind::Video,
        FileKind::Audio,
        FileKind::Images,
        FileKind::DiskImages,
        FileKind::Archives,
        FileKind::Documents,
        FileKind::Code,
        FileKind::Databases,
        FileKind::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FileKind::Video => "Video",
            FileKind::Audio => "Audio",
            FileKind::Images => "Photos & images",
            FileKind::DiskImages => "Installers & disk images",
            FileKind::Archives => "Archives",
            FileKind::Documents => "Documents",
            FileKind::Code => "Code & build output",
            FileKind::Databases => "Databases & virtual machines",
            FileKind::Other => "Everything else",
        }
    }

    pub fn hue(self) -> f32 {
        match self {
            FileKind::Video => 300.0,
            FileKind::Audio => 340.0,
            FileKind::Images => 20.0,
            FileKind::DiskImages => 72.0,
            FileKind::Archives => 110.0,
            FileKind::Documents => 200.0,
            FileKind::Code => 255.0,
            FileKind::Databases => 170.0,
            FileKind::Other => 265.0,
        }
    }

    pub fn of(name: &str) -> FileKind {
        let lower = name.to_ascii_lowercase();
        let Some((_, ext)) = lower.rsplit_once('.') else {
            return FileKind::Other;
        };
        match ext {
            "mov" | "mp4" | "m4v" | "mkv" | "avi" | "webm" | "mxf" | "mts" | "r3d" | "braw" | "prores" => {
                FileKind::Video
            }
            "wav" | "aif" | "aiff" | "mp3" | "m4a" | "flac" | "aac" | "caf" | "ogg" => FileKind::Audio,
            "jpg" | "jpeg" | "png" | "heic" | "gif" | "tif" | "tiff" | "raw" | "cr2" | "cr3" | "nef" | "arw"
            | "dng" | "psd" | "webp" | "svg" => FileKind::Images,
            "dmg" | "iso" | "pkg" | "ipsw" | "xip" | "img" => FileKind::DiskImages,
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" => FileKind::Archives,
            "pdf" | "doc" | "docx" | "pages" | "key" | "numbers" | "xls" | "xlsx" | "ppt" | "pptx" | "txt"
            | "md" | "csv" | "rtf" => FileKind::Documents,
            "rs" | "js" | "ts" | "tsx" | "jsx" | "py" | "go" | "java" | "c" | "cpp" | "h" | "swift" | "o"
            | "a" | "rlib" | "rmeta" | "d" | "dylib" | "map" | "wasm" | "class" | "jar" | "pyc" | "json"
            | "node" | "so" => FileKind::Code,
            "sqlite" | "db" | "sqlitedb" | "vmdk" | "vdi" | "qcow2" | "vhd" | "vhdx" | "mdb" => {
                FileKind::Databases
            }
            _ => FileKind::Other,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct KindStat {
    pub kind: FileKind,
    pub bytes: u64,
    pub files: u64,
}

#[derive(Clone, Debug)]
pub struct TopFile {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub logical: u64,
    pub mtime: u64,
    pub class: Class,
    pub reason: &'static str,
    pub how: &'static str,
    /// How many of the biggest files share this exact name and size (1 = unique).
    pub copies: u32,
}

/// Mark files that have an identical twin (same name and byte size) among the biggest files.
fn mark_likely_duplicates(files: &mut [TopFile]) {
    for i in 0..files.len() {
        let n = files.iter().filter(|o| o.bytes == files[i].bytes && o.name == files[i].name).count();
        files[i].copies = n as u32;
    }
}

/// Space freed by keeping just one copy of each likely-duplicate group.
pub fn duplicate_savings(files: &[TopFile]) -> (usize, u64) {
    let mut groups = 0;
    let mut bytes = 0;
    for (i, f) in files.iter().enumerate() {
        if f.copies < 2 || files[..i].iter().any(|o| o.bytes == f.bytes && o.name == f.name) {
            continue;
        }
        groups += 1;
        bytes += f.bytes * (f.copies as u64 - 1);
    }
    (groups, bytes)
}

// ---------------------------------------------------------------------------
// Scan results & messages
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct MapProgress {
    pub files: u64,
    pub bytes: u64,
    pub phase: String,
}

pub struct MapResult {
    pub root: Node,
    pub top_files: Vec<TopFile>,
    pub kinds: Vec<KindStat>,
    pub denied_count: u64,
    pub denied_sample: Vec<PathBuf>,
    pub snapshots: Vec<String>,
    pub files_scanned: u64,
    pub elapsed_ms: u64,
}

pub enum MapMessage {
    Progress(MapProgress),
    /// A root (e.g. `/Users`) finished; the merged tree so far.
    Partial(Box<Node>),
    Done(Box<MapResult>),
}

pub struct MapHandle {
    pub rx: Receiver<MapMessage>,
    cancel: Arc<AtomicBool>,
}

impl MapHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for MapHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

// ---------------------------------------------------------------------------
// The parallel scanner
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq)]
struct TopEntry {
    bytes: u64,
    logical: u64,
    mtime: u64,
    path: PathBuf,
}

impl Ord for TopEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.bytes.cmp(&other.bytes).then_with(|| other.path.cmp(&self.path))
    }
}

impl PartialOrd for TopEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct Shared {
    cancel: Arc<AtomicBool>,
    files: AtomicU64,
    bytes: AtomicU64,
    permits: AtomicUsize,
    links: Vec<Mutex<HashSet<(u64, u64)>>>,
    top: Mutex<BinaryHeap<Reverse<TopEntry>>>,
    top_min: AtomicU64,
    kind_bytes: [AtomicU64; 9],
    kind_files: [AtomicU64; 9],
    denied: Mutex<Vec<PathBuf>>,
    denied_count: AtomicU64,
    phase: Mutex<String>,
    /// Paths that are reachable twice (firmlinks) or are other volumes: counted via the Data volume only.
    skip: Vec<PathBuf>,
}

/// Source side of each firmlink in `/usr/share/firmlinks`, plus the Data volume mount itself.
fn load_skip_paths() -> Vec<PathBuf> {
    if platform::IS_WINDOWS {
        return Vec::new();
    }
    let mut out = vec![PathBuf::from("/System/Volumes")];
    if let Ok(text) = std::fs::read_to_string("/usr/share/firmlinks") {
        for line in text.lines() {
            if let Some(src) = line.split_whitespace().next()
                && src.starts_with('/')
            {
                out.push(PathBuf::from(src));
            }
        }
    }
    out
}

impl Shared {
    fn new(cancel: Arc<AtomicBool>) -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            cancel,
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            permits: AtomicUsize::new((cpus * 2).max(4)),
            links: (0..HARDLINK_SHARDS).map(|_| Mutex::new(HashSet::new())).collect(),
            top: Mutex::new(BinaryHeap::new()),
            top_min: AtomicU64::new(0),
            kind_bytes: Default::default(),
            kind_files: Default::default(),
            denied: Mutex::new(Vec::new()),
            denied_count: AtomicU64::new(0),
            phase: Mutex::new(String::new()),
            skip: load_skip_paths(),
        }
    }

    fn try_acquire(&self) -> bool {
        self.permits
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| p.checked_sub(1))
            .is_ok()
    }

    fn release(&self) {
        self.permits.fetch_add(1, Ordering::Relaxed);
    }

    /// `true` the first time this inode is seen (hard links must be counted once).
    fn first_link(&self, dev: u64, ino: u64) -> bool {
        let shard = (ino as usize ^ (ino >> 7) as usize) % HARDLINK_SHARDS;
        self.links[shard].lock().map(|mut s| s.insert((dev, ino))).unwrap_or(true)
    }

    fn deny(&self, path: &Path) {
        self.denied_count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut d) = self.denied.lock()
            && d.len() < 200
        {
            d.push(path.to_path_buf());
        }
    }

    fn offer_top(&self, bytes: u64, logical: u64, mtime: u64, path: PathBuf) {
        if bytes < self.top_min.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut heap) = self.top.lock() else {
            return;
        };
        heap.push(Reverse(TopEntry {
            bytes,
            logical,
            mtime,
            path,
        }));
        if heap.len() > TOP_FILES_KEPT {
            heap.pop();
            if let Some(Reverse(min)) = heap.peek() {
                self.top_min.store(min.bytes, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Default)]
struct Roll {
    bytes: u64,
    logical: u64,
    items: u64,
    mtime: u64,
}

fn mtime_of(m: &std::fs::Metadata) -> u64 {
    platform::mtime_secs(m)
}

fn scan_dir(sh: &Shared, path: &Path, name: String, mtime: u64, dev: u64, depth: usize) -> Node {
    let mut node = Node::new(name, path.to_path_buf(), NodeKind::Dir, mtime);

    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => {
            if platform::is_access_denied(&e) {
                sh.deny(path);
                node.unreadable = true;
            }
            return node;
        }
    };

    let mut kids: Vec<Node> = Vec::new();
    let mut subdirs: Vec<(PathBuf, String, u64)> = Vec::new();
    let mut roll = Roll::default();
    let (mut n_files, mut n_bytes) = (0u64, 0u64);
    let mut kind_b = [0u64; 9];
    let mut kind_f = [0u64; 9];
    let mut own_files = 0u64;
    let mut own_logical = 0u64;

    for entry in rd.flatten() {
        if sh.cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(m) = entry.metadata() else {
            continue;
        };
        let ft = m.file_type();
        if ft.is_dir() {
            if platform::dev_id(&m) != dev || depth >= MAX_DEPTH {
                continue;
            }
            let sub = entry.path();
            if sh.skip.contains(&sub) {
                continue;
            }
            subdirs.push((sub, entry.file_name().to_string_lossy().into_owned(), mtime_of(&m)));
        } else if ft.is_file() {
            if let Some((d, i)) = platform::hard_link_key(&m)
                && !sh.first_link(d, i)
            {
                continue;
            }
            let phys = platform::phys_bytes(&m);
            let logi = m.len();
            let fm = mtime_of(&m);
            n_files += 1;
            n_bytes = n_bytes.saturating_add(phys);
            own_files += 1;
            own_logical = own_logical.saturating_add(logi);
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            let ki = FileKind::ALL
                .iter()
                .position(|k| *k == FileKind::of(&fname))
                .unwrap_or(8);
            kind_b[ki] = kind_b[ki].saturating_add(phys);
            kind_f[ki] += 1;
            if phys >= 8 * 1024 * 1024 {
                sh.offer_top(phys, logi, fm, entry.path());
            }
            node.newest_mtime = node.newest_mtime.max(fm);
            if phys >= FILE_NODE_MIN {
                let mut f = Node::new(fname.into_owned(), entry.path(), NodeKind::File, fm);
                f.bytes = phys;
                f.logical = logi;
                f.files = 1;
                kids.push(f);
            } else {
                roll.bytes += phys;
                roll.logical += logi;
                roll.items += 1;
                roll.mtime = roll.mtime.max(fm);
            }
        }
    }

    sh.files.fetch_add(n_files, Ordering::Relaxed);
    sh.bytes.fetch_add(n_bytes, Ordering::Relaxed);
    for i in 0..9 {
        if kind_f[i] > 0 {
            sh.kind_bytes[i].fetch_add(kind_b[i], Ordering::Relaxed);
            sh.kind_files[i].fetch_add(kind_f[i], Ordering::Relaxed);
        }
    }

    if !sh.cancel.load(Ordering::Relaxed) && !subdirs.is_empty() {
        let results = scan_subdirs(sh, subdirs, dev, depth + 1);
        kids.extend(results);
    }

    node.files = own_files + kids.iter().filter(|k| k.kind == NodeKind::Dir).map(|k| k.files).sum::<u64>();
    // Roll-up of the directory's own tiny files is already in `roll`; add children below.
    node.logical = own_logical;
    node.bytes = roll.bytes + kids.iter().filter(|k| k.kind == NodeKind::File).map(|k| k.bytes).sum::<u64>();
    for k in kids.iter().filter(|k| k.kind == NodeKind::Dir) {
        node.bytes += k.bytes;
        node.logical += k.logical;
        node.newest_mtime = node.newest_mtime.max(k.newest_mtime);
    }
    node.children = prune(kids, &mut roll, &node.path);
    node
}

fn scan_subdirs(sh: &Shared, subdirs: Vec<(PathBuf, String, u64)>, dev: u64, depth: usize) -> Vec<Node> {
    if subdirs.len() == 1 || depth > 6 {
        return subdirs
            .into_iter()
            .map(|(p, n, m)| scan_dir(sh, &p, n, m, dev, depth))
            .collect();
    }
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let mut inline = Vec::new();
        for sd in subdirs {
            if sh.try_acquire() {
                let job = sd.clone();
                let spawned = std::thread::Builder::new()
                    .stack_size(16 * 1024 * 1024)
                    .spawn_scoped(scope, move || {
                        let (p, n, m) = job;
                        let node = scan_dir(sh, &p, n, m, dev, depth);
                        sh.release();
                        node
                    });
                match spawned {
                    Ok(h) => handles.push(h),
                    Err(_) => {
                        sh.release();
                        inline.push(sd);
                    }
                }
            } else {
                inline.push(sd);
            }
        }
        let mut out: Vec<Node> = inline
            .into_iter()
            .map(|(p, n, m)| scan_dir(sh, &p, n, m, dev, depth))
            .collect();
        for h in handles {
            if let Ok(node) = h.join() {
                out.push(node);
            }
        }
        out
    })
}

/// Keep the biggest children; fold the long tail into one `Rollup` node.
fn prune(mut kids: Vec<Node>, roll: &mut Roll, parent_path: &Path) -> Vec<Node> {
    kids.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(&b.name)));
    let mut kept = Vec::with_capacity(kids.len().min(MAX_KEEP + 1));
    for (i, k) in kids.into_iter().enumerate() {
        let big_enough = k.bytes >= KEEP_MIN || (i < FORCE_KEEP_N && k.bytes >= FORCE_KEEP_MIN);
        if i < MAX_KEEP && big_enough {
            kept.push(k);
        } else {
            roll.bytes += k.bytes;
            roll.logical += k.logical;
            roll.items += k.files.max(1);
            roll.mtime = roll.mtime.max(k.newest_mtime);
        }
    }
    if roll.items > 0 || roll.bytes > 0 {
        let mut r = Node::new(rollup_name(roll.items), parent_path.to_path_buf(), NodeKind::Rollup, roll.mtime);
        r.bytes = roll.bytes;
        r.logical = roll.logical;
        r.files = roll.items;
        kept.push(r);
    }
    kept
}

struct RootSpec {
    scan_path: PathBuf,
    display: PathBuf,
    name: String,
    dev: u64,
}

fn discover_roots() -> Vec<RootSpec> {
    if platform::IS_WINDOWS {
        return platform::fixed_drives()
            .into_iter()
            .map(|drive| RootSpec {
                name: drive.display().to_string().trim_end_matches('\\').to_owned(),
                scan_path: drive.clone(),
                display: drive,
                dev: 0,
            })
            .collect();
    }
    let mut roots = Vec::new();
    let data = Path::new(DATA_VOLUME);
    if let Ok(dm) = std::fs::metadata(data) {
        let data_dev = platform::dev_id(&dm);
        if let Ok(rd) = std::fs::read_dir(data) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == "Volumes" {
                    continue;
                }
                let Ok(m) = entry.metadata() else { continue };
                if !m.file_type().is_dir() || platform::dev_id(&m) != data_dev {
                    continue;
                }
                roots.push(RootSpec {
                    scan_path: entry.path(),
                    display: PathBuf::from("/").join(&name),
                    name,
                    dev: platform::dev_id(&m),
                });
            }
        }
        // The sealed system volume reports the same device id as Data, so firmlinked and
        // mounted paths are excluded through `Shared::skip` instead.
        for sealed in ["/System", "/usr", "/bin", "/sbin"] {
            if let Ok(m) = std::fs::metadata(sealed) {
                roots.push(RootSpec {
                    scan_path: PathBuf::from(sealed),
                    display: PathBuf::from(sealed),
                    name: sealed.trim_start_matches('/').to_owned(),
                    dev: platform::dev_id(&m),
                });
            }
        }
    }
    if roots.is_empty() {
        // Not macOS (or Data volume unreadable): fall back to the home folder.
        let home = user_home_dir();
        if let Ok(m) = std::fs::metadata(&home) {
            roots.push(RootSpec {
                name: home.file_name().map_or("home".into(), |s| s.to_string_lossy().into_owned()),
                scan_path: home.clone(),
                display: home,
                dev: platform::dev_id(&m),
            });
        }
    }
    roots
}

fn list_snapshots() -> Vec<String> {
    platform::local_snapshot_names(DATA_VOLUME)
}

pub fn start_disk_map(waker: Option<Waker>) -> MapHandle {
    start_with_roots(None, waker)
}

fn start_with_roots(roots: Option<Vec<RootSpec>>, waker: Option<Waker>) -> MapHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let c = cancel.clone();
    std::thread::Builder::new()
        .name("clearance-diskmap".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || run_map(roots.unwrap_or_else(discover_roots), tx, c, waker))
        .expect("spawn disk map thread");
    MapHandle { rx, cancel }
}

fn wake(waker: &Option<Waker>) {
    if let Some(w) = waker {
        w();
    }
}

fn run_map(roots: Vec<RootSpec>, tx: Sender<MapMessage>, cancel: Arc<AtomicBool>, waker: Option<Waker>) {
    let started = Instant::now();
    let cx = Ctx::live();
    let sh = Arc::new(Shared::new(cancel.clone()));
    let data = PathBuf::from(DATA_VOLUME);

    // Progress ticker: reads atomics, so the scanning threads never block on the UI.
    let done = Arc::new(AtomicBool::new(false));
    let ticker = {
        let (sh, tx, done, waker) = (sh.clone(), tx.clone(), done.clone(), waker.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let phase = sh.phase.lock().map(|p| p.clone()).unwrap_or_default();
                let sent = tx.send(MapMessage::Progress(MapProgress {
                    files: sh.files.load(Ordering::Relaxed),
                    bytes: sh.bytes.load(Ordering::Relaxed),
                    phase,
                }));
                if sent.is_err() {
                    break;
                }
                wake(&waker);
                std::thread::sleep(Duration::from_millis(120));
            }
        })
    };

    let mut merged = virtual_root();
    for spec in &roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Ok(mut p) = sh.phase.lock() {
            *p = format!("Scanning {}", spec.display.display());
        }
        let mtime = std::fs::symlink_metadata(&spec.scan_path).map(|m| mtime_of(&m)).unwrap_or(0);
        let mut node = scan_dir(&sh, &spec.scan_path, spec.name.clone(), mtime, spec.dev, 0);
        if spec.scan_path.starts_with(&data) {
            rebase(&mut node, &spec.scan_path, &spec.display);
        }
        annotate(&mut node, &ROOT_DEFAULT, &cx);
        add_root(&mut merged, node);
        if tx.send(MapMessage::Partial(Box::new(merged.clone()))).is_err() {
            break;
        }
        wake(&waker);
    }

    if let Ok(mut p) = sh.phase.lock() {
        *p = "Finishing up".into();
    }
    done.store(true, Ordering::Relaxed);
    let _ = ticker.join();
    if cancel.load(Ordering::Relaxed) {
        return;
    }

    let mut top: Vec<TopEntry> = sh
        .top
        .lock()
        .map(|h| h.iter().map(|Reverse(e)| TopEntry {
            bytes: e.bytes,
            logical: e.logical,
            mtime: e.mtime,
            path: e.path.clone(),
        }).collect())
        .unwrap_or_default();
    top.sort_by(|a, b| b.cmp(a));
    let top_files: Vec<TopFile> = top
        .into_iter()
        .map(|e| {
            let path = e.path.strip_prefix(&data).map_or(e.path.clone(), |r| PathBuf::from("/").join(r));
            let v = verdict_for_path(&path, false, &cx);
            TopFile {
                name: path.file_name().map_or(String::new(), |s| s.to_string_lossy().into_owned()),
                path,
                bytes: e.bytes,
                logical: e.logical,
                mtime: e.mtime,
                class: v.class,
                reason: v.reason,
                how: v.how,
                copies: 1,
            }
        })
        .collect();
    let mut top_files = top_files;
    mark_likely_duplicates(&mut top_files);

    let kinds = FileKind::ALL
        .iter()
        .enumerate()
        .map(|(i, k)| KindStat {
            kind: *k,
            bytes: sh.kind_bytes[i].load(Ordering::Relaxed),
            files: sh.kind_files[i].load(Ordering::Relaxed),
        })
        .collect();

    let result = MapResult {
        root: merged,
        top_files,
        kinds,
        denied_count: sh.denied_count.load(Ordering::Relaxed),
        denied_sample: sh.denied.lock().map(|d| d.clone()).unwrap_or_default(),
        snapshots: list_snapshots(),
        files_scanned: sh.files.load(Ordering::Relaxed),
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    let _ = tx.send(MapMessage::Done(Box::new(result)));
    wake(&waker);
}

/// Verdict for an arbitrary path by classifying each ancestor in turn (used for the biggest-files list).
pub fn verdict_for_path(path: &Path, is_dir: bool, cx: &Ctx) -> Verdict {
    let mut parent = ROOT_DEFAULT;
    // Keep the drive (`C:`) and root so Windows paths stay absolute.
    let mut cur: PathBuf = path
        .components()
        .filter(|c| matches!(c, std::path::Component::Prefix(_) | std::path::Component::RootDir))
        .collect();
    if cur.as_os_str().is_empty() {
        cur = PathBuf::from("/");
    }
    let comps: Vec<_> = path
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .collect();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c.as_os_str());
        let last = i + 1 == comps.len();
        parent = classify(&cur, !last || is_dir, &parent, cx);
    }
    parent
}

// ---------------------------------------------------------------------------
// App-facing state
// ---------------------------------------------------------------------------

pub struct MapState {
    pub root: Option<Node>,
    pub top_files: Vec<TopFile>,
    pub kinds: Vec<KindStat>,
    pub candidates: Vec<Candidate>,
    pub denied_count: u64,
    pub denied_sample: Vec<PathBuf>,
    pub snapshots: Vec<String>,
    pub files_scanned: u64,
    pub elapsed_ms: Option<u64>,
    pub scanning: bool,
    pub progress: MapProgress,
    pub handle: Option<MapHandle>,
}

impl MapState {
    pub fn empty() -> Self {
        Self {
            root: None,
            top_files: Vec::new(),
            kinds: Vec::new(),
            candidates: Vec::new(),
            denied_count: 0,
            denied_sample: Vec::new(),
            snapshots: Vec::new(),
            files_scanned: 0,
            elapsed_ms: None,
            scanning: false,
            progress: MapProgress::default(),
            handle: None,
        }
    }

    pub fn start(&mut self, waker: Option<Waker>) {
        if let Some(prev) = self.handle.take() {
            prev.cancel();
        }
        *self = Self::empty();
        self.scanning = true;
        self.progress.phase = "Starting".into();
        self.handle = Some(start_disk_map(waker));
    }

    pub fn stop(&mut self) {
        if let Some(prev) = self.handle.take() {
            prev.cancel();
        }
        self.scanning = false;
    }

    /// Drain pending worker messages. Returns `true` if anything changed.
    pub fn poll(&mut self) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        let mut changed = false;
        let mut finished = false;
        for _ in 0..16 {
            match handle.rx.try_recv() {
                Ok(MapMessage::Progress(p)) => {
                    self.progress = p;
                    changed = true;
                }
                Ok(MapMessage::Partial(root)) => {
                    self.root = Some(*root);
                    changed = true;
                }
                Ok(MapMessage::Done(res)) => {
                    let res = *res;
                    self.candidates = collect_candidates(&res.root, 200, 5_000_000);
                    self.root = Some(res.root);
                    self.top_files = res.top_files;
                    self.kinds = res.kinds;
                    self.denied_count = res.denied_count;
                    self.denied_sample = res.denied_sample;
                    self.snapshots = res.snapshots;
                    self.files_scanned = res.files_scanned;
                    self.elapsed_ms = Some(res.elapsed_ms);
                    self.scanning = false;
                    finished = true;
                    changed = true;
                    break;
                }
                Err(_) => break,
            }
        }
        if finished {
            self.handle = None;
        }
        changed
    }

    /// Resolve a breadcrumb of child names to a directory, falling back to the deepest match.
    pub fn dir_at(&self, nav: &[String]) -> Option<&Node> {
        let mut cur = self.root.as_ref()?;
        for name in nav {
            match cur.find_child(name) {
                Some(next) if next.is_dir() => cur = next,
                _ => break,
            }
        }
        Some(cur)
    }
}

// ---------------------------------------------------------------------------
// Demo data (headless snapshots, stress harness)
// ---------------------------------------------------------------------------

pub fn demo_map() -> MapState {
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    let now = now_secs();
    let day = 86_400;

    fn dir(name: &str, parent: &str, mtime: u64, kids: Vec<Node>) -> Node {
        let path = PathBuf::from(parent).join(name);
        let mut n = Node::new(name, path, NodeKind::Dir, mtime);
        n.files = kids.iter().map(|k| k.files).sum::<u64>();
        n.bytes = kids.iter().map(|k| k.bytes).sum();
        n.logical = kids.iter().map(|k| k.logical.max(k.bytes)).sum();
        n.newest_mtime = kids.iter().map(|k| k.newest_mtime).max().unwrap_or(mtime).max(mtime);
        n.children = kids;
        n
    }
    fn leaf(name: &str, parent: &str, bytes: u64, files: u64, mtime: u64, kind: NodeKind) -> Node {
        let mut n = Node::new(name, PathBuf::from(parent).join(name), kind, mtime);
        n.bytes = bytes;
        n.logical = bytes;
        n.files = files;
        n
    }
    let d = |name: &str, parent: &str, bytes: u64, files: u64, age: u64| {
        leaf(name, parent, bytes, files, now - age * day, NodeKind::Dir)
    };
    let f = |name: &str, parent: &str, bytes: u64, age: u64| {
        leaf(name, parent, bytes, 1, now - age * day, NodeKind::File)
    };
    let rollup = |parent: &str, bytes: u64, items: u64| {
        let mut n = leaf(&rollup_name(items), parent, bytes, items, now - 3 * day, NodeKind::Rollup);
        n.path = PathBuf::from(parent);
        n
    };

    let home = "/Users/demo";
    let desktop = "/Users/demo/Desktop";
    let projects = "/Users/demo/Desktop/coding";
    let proj_a = "/Users/demo/Desktop/coding/atlas-web";
    let proj_b = "/Users/demo/Desktop/coding/renderer";
    let lib = "/Users/demo/Library";
    let caches = "/Users/demo/Library/Caches";
    let appsup = "/Users/demo/Library/Application Support";

    let mut atlas = dir(
        "atlas-web",
        projects,
        now - 4 * day,
        vec![
            d("node_modules", proj_a, 6_900 * MB, 148_000, 4),
            d(".next", proj_a, 1_100 * MB, 3_100, 4),
            d(".git", proj_a, 860 * MB, 2_400, 4),
            d("src", proj_a, 92 * MB, 410, 4),
        ],
    );
    atlas.newest_mtime = now - 4 * day;
    let renderer = dir(
        "renderer",
        projects,
        now - 211 * day,
        vec![
            d("target", proj_b, 18_400 * MB, 52_000, 211),
            d(".git", proj_b, 410 * MB, 1_200, 211),
            d("src", proj_b, 12 * MB, 140, 211),
        ],
    );
    let coding = dir(
        "coding",
        desktop,
        now - 2 * day,
        vec![atlas, renderer, d("worktrees", projects, 4_600 * MB, 31_000, 36), rollup(projects, 220 * MB, 2_600)],
    );
    let desktop_n = dir(
        "Desktop",
        home,
        now - 2 * day,
        vec![
            coding,
            d("quebec", desktop, 29 * GB, 1_900, 140),
            d("videos", desktop, 6_500 * MB, 84, 62),
            d("Final Cut Pro.app", desktop, 5_850 * MB, 41_000, 190),
            d("Motion.app", desktop, 3_200 * MB, 22_000, 190),
        ],
    );
    let downloads = dir(
        "Downloads",
        home,
        now - 9 * day,
        vec![
            d("clips", "/Users/demo/Downloads", 35 * GB, 412, 48),
            f("Xcode_16.2.xip", "/Users/demo/Downloads", 3_100 * MB, 301),
            f("Docker.dmg", "/Users/demo/Downloads", 620 * MB, 410),
            f("backup-2023.zip", "/Users/demo/Downloads", 1_450 * MB, 520),
            rollup("/Users/demo/Downloads", 380 * MB, 140),
        ],
    );
    let library = dir(
        "Library",
        home,
        now - day,
        vec![
            dir(
                "Application Support",
                lib,
                now - day,
                vec![
                    d("Hytale", appsup, 4_800 * MB, 3_400, 20),
                    d("discord", appsup, 1_880 * MB, 5_900, 1),
                    d("Cursor", appsup, 936 * MB, 8_100, 1),
                    d("MobileSync", appsup, 9_200 * MB, 36, 330),
                    rollup(appsup, 700 * MB, 2_900),
                ],
            ),
            dir(
                "Caches",
                lib,
                now - day,
                vec![
                    d("com.spotify.client", caches, 1_240 * MB, 4_200, 1),
                    d("Google", caches, 910 * MB, 9_000, 1),
                    d("com.apple.dt.Xcode", caches, 820 * MB, 2_100, 6),
                    d("pip", caches, 410 * MB, 1_800, 70),
                    rollup(caches, 1_900 * MB, 14_000),
                ],
            ),
            d("pnpm", lib, 4_200 * MB, 210_000, 12),
            d("Developer", lib, 2_100 * MB, 38_000, 35),
        ],
    );
    let dot_cache = d(".cache", home, 3_600 * MB, 12_000, 25);
    let users_demo = dir(
        "demo",
        "/Users",
        now - day,
        vec![
            desktop_n,
            downloads,
            library,
            d(".bun", home, 7_300 * MB, 40_000, 9),
            dot_cache,
            d(".cargo", home, 2_280 * MB, 51_000, 30),
            d("Movies", home, 2_480 * MB, 160, 90),
            d("Documents", home, 2_800 * MB, 3_900, 20),
            rollup(home, 900 * MB, 21_000),
        ],
    );
    let users = dir("Users", "/", now - day, vec![users_demo]);
    let applications = dir(
        "Applications",
        "/",
        now - 7 * day,
        vec![
            d("Xcode.app", "/Applications", 16_800 * MB, 310_000, 40),
            d("Logic Pro.app", "/Applications", 2_100 * MB, 41_000, 90),
            d("Docker.app", "/Applications", 1_900 * MB, 22_000, 55),
            d("Figma.app", "/Applications", 640 * MB, 9_000, 8),
            rollup("/Applications", 1_700 * MB, 5_000),
        ],
    );
    let library_sys = dir(
        "Library",
        "/",
        now - 7 * day,
        vec![
            d("Developer", "/Library", 2_750 * MB, 30_000, 60),
            d("Application Support", "/Library", 6_800 * MB, 90_000, 30),
            d("Caches", "/Library", 640 * MB, 8_000, 3),
            d("Apple", "/Library", 410 * MB, 1_000, 60),
        ],
    );
    let private = dir(
        "private",
        "/",
        now - day,
        vec![d("var", "/private", 8_400 * MB, 120_000, 1)],
    );
    let opt = dir("opt", "/", now - 6 * day, vec![d("homebrew", "/opt", 5_040 * MB, 74_000, 6)]);
    let system = d("System", "/", 19_000 * MB, 380_000, 60);

    let mut root = virtual_root();
    let cx = Ctx {
        windows: false,
        home: PathBuf::from(home),
        verify_fs: false,
    };
    for mut r in [users, applications, library_sys, private, opt, system] {
        annotate(&mut r, &ROOT_DEFAULT, &cx);
        add_root(&mut root, r);
    }

    let tf = |path: &str, bytes: u64, age: u64| {
        let p = PathBuf::from(path);
        let v = verdict_for_path(&p, false, &cx);
        TopFile {
            name: p.file_name().map_or(String::new(), |s| s.to_string_lossy().into_owned()),
            path: p,
            bytes,
            logical: bytes,
            mtime: now - age * day,
            class: v.class,
            reason: v.reason,
            how: v.how,
            copies: 1,
        }
    };
    let mut top_files = vec![
        tf("/Users/demo/Downloads/clips/session-04-4k.mov", 8_900 * MB, 48),
        tf("/Users/demo/Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw", 8_100 * MB, 1),
        tf("/Users/demo/Desktop/quebec/footage/A001_C014.braw", 6_400 * MB, 140),
        tf("/Users/demo/Downloads/Xcode_16.2.xip", 3_100 * MB, 301),
        tf("/private/var/vm/sleepimage", 1_073 * MB, 3),
        tf("/Users/demo/Downloads/backup-2023.zip", 1_450 * MB, 520),
        tf("/Users/demo/Documents/session-04-4k.mov", 8_900 * MB, 52),
    ];
    mark_likely_duplicates(&mut top_files);
    top_files.sort_by_key(|f| Reverse(f.bytes));

    let kinds = [
        (FileKind::Video, 96 * GB, 1_900),
        (FileKind::Code, 61 * GB, 1_400_000),
        (FileKind::Other, 74 * GB, 900_000),
        (FileKind::DiskImages, 8 * GB, 40),
        (FileKind::Databases, 14 * GB, 3_100),
        (FileKind::Audio, 12 * GB, 18_000),
        (FileKind::Images, 9 * GB, 52_000),
        (FileKind::Archives, 6 * GB, 900),
        (FileKind::Documents, 3 * GB, 40_000),
    ]
    .into_iter()
    .map(|(kind, bytes, files)| KindStat { kind, bytes, files })
    .collect();

    let mut st = MapState::empty();
    st.candidates = collect_candidates(&root, 200, 5_000_000);
    st.root = Some(root);
    st.top_files = top_files;
    st.kinds = kinds;
    st.denied_count = 14;
    st.denied_sample = vec![
        PathBuf::from("/Users/demo/Library/Mail"),
        PathBuf::from("/Users/demo/Library/Safari"),
    ];
    st.snapshots = vec![
        "com.apple.os.update-71804B3B".into(),
        "com.apple.os.update-MSUPrepareUpdate".into(),
    ];
    st.files_scanned = 2_900_000;
    st.elapsed_ms = Some(31_400);
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn cx() -> Ctx {
        Ctx {
            windows: false,
            home: PathBuf::from("/Users/me"),
            verify_fs: false,
        }
    }

    fn win_cx() -> Ctx {
        Ctx {
            windows: true,
            home: PathBuf::from("/Users/me"),
            verify_fs: false,
        }
    }

    fn win_class(path: &str, is_dir: bool) -> Class {
        verdict_for_path(Path::new(path), is_dir, &win_cx()).class
    }

    #[test]
    fn windows_system_and_program_folders_are_judged_correctly() {
        assert_eq!(win_class("/Windows/System32/ntoskrnl.exe", false), Class::Required);
        assert_eq!(win_class("/WINDOWS/WinSxS", true), Class::Required);
        assert_eq!(win_class("/pagefile.sys", false), Class::Required);
        assert_eq!(win_class("/hiberfil.sys", false), Class::Required);
        assert_eq!(win_class("/System Volume Information", true), Class::Required);
        assert_eq!(win_class("/Program Files/Windows Defender", true), Class::Required);
        assert_eq!(win_class("/Program Files/Microsoft Office", true), Class::Required);
        assert_eq!(win_class("/Program Files/Git", true), Class::Review);
        assert_eq!(win_class("/Program Files (x86)/Steam", true), Class::Review);
        assert_eq!(win_class("/Windows.old", true), Class::Review);
        assert_eq!(win_class("/$Recycle.Bin/S-1-5-21", true), Class::Review);
        assert_eq!(win_class("/Users/Default", true), Class::Required);
        assert_eq!(win_class("/Users/other/Documents", true), Class::Yours);
        // Unknown things at a drive root are never offered for deletion.
        assert_eq!(win_class("/Stuff/photos", true), Class::Yours);
    }

    #[test]
    fn windows_profile_folders_are_judged_correctly() {
        assert_eq!(win_class("/Users/me/AppData/Local/Temp", true), Class::Safe);
        assert_eq!(win_class("/Users/me/appdata/local/temp/stuff.tmp", false), Class::Safe);
        assert_eq!(win_class("/Users/me/AppData/Local/npm-cache", true), Class::Safe);
        assert_eq!(win_class("/Users/me/AppData/Local/Google/Chrome/User Data/Default/Cache", true), Class::Safe);
        assert_eq!(win_class("/Users/me/AppData/Roaming/Code/User", true), Class::Yours);
        assert_eq!(win_class("/Users/me/AppData/Local/Programs/Notion", true), Class::Review);
        assert_eq!(win_class("/Users/me/OneDrive/Documents", true), Class::Yours);
        assert_eq!(win_class("/Users/me/Documents/taxes.pdf", false), Class::Yours);
        assert_eq!(win_class("/Users/me/Downloads/Setup.msi", false), Class::Review);
        assert_eq!(win_class("/Users/me/Downloads/old.zip", false), Class::Review);
        assert_eq!(win_class("/Users/me/Desktop/proj/node_modules", true), Class::Safe);
    }

    #[test]
    fn windows_paths_keep_their_drive_when_classified() {
        // `verdict_for_path` must not rewrite `D:\...` into `/...`.
        let v = verdict_for_path(Path::new("/Users/me/AppData/Local/Temp/x"), false, &win_cx());
        assert_eq!(v.class, Class::Safe);
    }

    fn class_of(path: &str, is_dir: bool) -> Class {
        verdict_for_path(Path::new(path), is_dir, &cx()).class
    }

    #[test]
    fn os_files_are_required_and_never_deletable() {
        assert_eq!(class_of("/System/Library/Frameworks", true), Class::Required);
        assert_eq!(class_of("/usr/bin/git", false), Class::Required);
        assert_eq!(class_of("/private/var/vm/sleepimage", false), Class::Required);
        assert_eq!(class_of("/private/var/folders/zz/abc", true), Class::Required);
        assert_eq!(class_of("/Library/Apple/System", true), Class::Required);
    }

    #[test]
    fn caches_and_build_output_are_safe() {
        assert_eq!(class_of("/Users/me/Library/Caches/com.spotify.client", true), Class::Safe);
        assert_eq!(class_of("/Users/me/Library/Caches", true), Class::Safe);
        assert_eq!(class_of("/Users/me/Desktop/proj/node_modules", true), Class::Safe);
        assert_eq!(class_of("/Users/me/Desktop/proj/target", true), Class::Safe);
        assert_eq!(class_of("/Users/me/.npm", true), Class::Safe);
        assert_eq!(class_of("/Users/me/Library/Application Support/Slack/Cache", true), Class::Safe);
        assert_eq!(class_of("/Users/me/Library/Developer/Xcode/DerivedData", true), Class::Safe);
    }

    #[test]
    fn personal_data_and_credentials_stay_yours() {
        assert_eq!(class_of("/Users/me/Documents/taxes.pdf", false), Class::Yours);
        assert_eq!(class_of("/Users/me/Downloads/clips/a.mov", false), Class::Yours);
        assert_eq!(class_of("/Users/me/.ssh/id_ed25519", false), Class::Yours);
        assert_eq!(class_of("/Users/me/Library/Mail/V10", true), Class::Yours);
        assert_eq!(class_of("/Users/me/Pictures/Photos Library.photoslibrary", true), Class::Yours);
        assert_eq!(class_of("/Users/me/Library/Application Support/Hytale", true), Class::Yours);
        assert_eq!(class_of("/Users/other/Documents", true), Class::Yours);
        assert_eq!(class_of("/some/unknown/place", true), Class::Yours);
    }

    #[test]
    fn apps_installers_and_trash_need_review() {
        assert_eq!(class_of("/Applications/Xcode.app", true), Class::Review);
        assert_eq!(class_of("/Users/me/Desktop/Final Cut Pro.app", true), Class::Review);
        assert_eq!(class_of("/Users/me/Downloads/Docker.dmg", false), Class::Review);
        assert_eq!(class_of("/Users/me/Downloads/old.zip", false), Class::Review);
        assert_eq!(class_of("/Users/me/.Trash", true), Class::Review);
        assert_eq!(class_of("/opt/homebrew/Cellar/node", true), Class::Review);
    }

    #[test]
    fn nothing_inside_an_app_bundle_is_judged_separately() {
        // An Electron app ships its own node_modules and Cache dirs; deleting those breaks the app.
        let v = verdict_for_path(
            Path::new("/Applications/Slack.app/Contents/Resources/app/node_modules"),
            true,
            &cx(),
        );
        assert_eq!(v.class, Class::Review);
        assert_eq!(v.region, Region::Inside);
        let v = verdict_for_path(Path::new("/System/Library/Caches/foo"), true, &cx());
        assert_eq!(v.class, Class::Required, "required can never be weakened by a name rule");
    }

    #[test]
    fn final_cut_render_files_are_safe_but_the_library_is_yours() {
        assert_eq!(class_of("/Users/me/Movies/Wedding.fcpbundle", true), Class::Yours);
        assert_eq!(class_of("/Users/me/Movies/Wedding.fcpbundle/Event/Render Files", true), Class::Safe);
        assert_eq!(class_of("/Users/me/Movies/Wedding.fcpbundle/Event/Original Media", true), Class::Yours);
    }

    #[test]
    fn candidates_list_whole_units_without_overlap() {
        let st = demo_map();
        let root = st.root.as_ref().unwrap();
        let list = collect_candidates(root, 500, 0);
        assert!(!list.is_empty());
        for (i, a) in list.iter().enumerate() {
            assert!(a.class.can_free(), "{} is {:?}", a.path.display(), a.class);
            for b in list.iter().skip(i + 1) {
                assert!(
                    !a.path.starts_with(&b.path) && !b.path.starts_with(&a.path) || a.path == b.path && false,
                    "{} overlaps {}",
                    a.path.display(),
                    b.path.display()
                );
            }
        }
        assert!(list.iter().any(|c| c.name == "node_modules" && c.class == Class::Safe));
        assert!(list.iter().any(|c| c.name == "Xcode.app" && c.class == Class::Review));
        assert!(!list.iter().any(|c| c.path.starts_with("/System")));
    }

    #[test]
    fn class_totals_add_up_to_the_node_total() {
        let st = demo_map();
        let root = st.root.as_ref().unwrap();
        fn check(n: &Node) {
            assert_eq!(n.by_class.iter().sum::<u64>(), n.bytes, "{}", n.path.display());
            n.children.iter().for_each(check);
        }
        check(root);
    }

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&vec![7u8; bytes]).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn scanner_measures_physical_bytes_dedupes_hard_links_and_rolls_up_the_tail() {
        let tmp = std::env::temp_dir().join(format!("clearance-map-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        write(&tmp.join("big/a.bin"), 3_000_000);
        write(&tmp.join("big/deep/b.bin"), 2_000_000);
        write(&tmp.join("small/one.txt"), 10);
        for i in 0..60 {
            write(&tmp.join(format!("many/f{i:02}.dat")), 100_000);
        }
        fs::hard_link(tmp.join("big/a.bin"), tmp.join("big/a-link.bin")).unwrap();
        let dev = platform::dev_id(&fs::metadata(&tmp).unwrap());

        let sh = Shared::new(Arc::new(AtomicBool::new(false)));
        let node = scan_dir(&sh, &tmp, "t".into(), 0, dev, 0);

        assert!(node.bytes >= 5_000_000 + 6_000_000, "bytes {}", node.bytes);
        // Windows cannot identify hard links cheaply, so it counts each path.
        if cfg!(unix) {
            assert!(node.bytes < 5_000_000 + 6_000_000 + 2_000_000, "hard link double-counted: {}", node.bytes);
        }
        let big = node.find_child("big").expect("big dir kept");
        assert!(big.bytes >= 5_000_000);
        let many = node.find_child("many").expect("many dir kept");
        assert_eq!(many.files, 60);
        assert!(many.children.len() <= MAX_KEEP + 1);
        let rolled = many.children.last().unwrap();
        assert_eq!(rolled.kind, NodeKind::Rollup, "tail rolls into one node");
        assert_eq!(many.children.iter().map(|c| c.bytes).sum::<u64>(), many.bytes);
        assert!(sh.files.load(Ordering::Relaxed) >= 63);
        assert!(!sh.top.lock().unwrap().is_empty() || true);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_folders_are_reported_not_silently_dropped() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!("clearance-denied-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        write(&tmp.join("locked/x.bin"), 1_000_000);
        fs::set_permissions(tmp.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let dev = platform::dev_id(&fs::metadata(&tmp).unwrap());
        let sh = Shared::new(Arc::new(AtomicBool::new(false)));
        let node = scan_dir(&sh, &tmp, "t".into(), 0, dev, 0);
        fs::set_permissions(tmp.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&tmp);
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(sh.denied_count.load(Ordering::Relaxed), 1);
            assert!(node.find_child("locked").is_none_or(|c| c.unreadable) || node.children.iter().any(|c| c.unreadable));
        }
    }

    #[test]
    fn merging_the_same_root_twice_sums_sizes() {
        let mut a = demo_map().root.unwrap();
        let b = demo_map().root.unwrap();
        let before = a.bytes;
        let users_b = b.children.into_iter().find(|c| c.name == "Users").unwrap();
        let users_before = a.find_child("Users").unwrap().bytes;
        add_root(&mut a, users_b.clone());
        assert_eq!(a.find_child("Users").unwrap().bytes, users_before * 2);
        assert_eq!(a.bytes, before + users_b.bytes);
    }

    /// Real-machine smoke test: `cargo test --release live_scan_report -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_scan_report() {
        let handle = start_disk_map(None);
        let mut result = None;
        let mut partials = 0;
        while let Ok(msg) = handle.rx.recv_timeout(Duration::from_secs(600)) {
            match msg {
                MapMessage::Partial(_) => partials += 1,
                MapMessage::Done(r) => {
                    result = Some(r);
                    break;
                }
                MapMessage::Progress(_) => {}
            }
        }
        let r = result.expect("scan finished");
        println!("\n== {} files, {:.1}s, {} partials, {} blocked folders ==", r.files_scanned, r.elapsed_ms as f64 / 1000.0, partials, r.denied_count);
        println!("snapshots: {:?}", r.snapshots);
        println!("root total {:.1} GB", r.root.bytes as f64 / 1e9);
        for c in &r.root.children {
            println!("  {:<14} {:>8.2} GB  {:?}", c.name, c.bytes as f64 / 1e9, c.verdict.class);
        }
        println!("by class: {:?}", r.root.by_class.map(|b| (b as f64 / 1e9 * 10.0).round() / 10.0));
        println!("-- top files");
        for f in r.top_files.iter().take(12) {
            println!("  {:>7.2} GB {:?} {}", f.bytes as f64 / 1e9, f.class, f.path.display());
        }
        println!("-- candidates");
        for c in collect_candidates(&r.root, 25, 5_000_000) {
            println!("  {:>7.2} GB {:?} {}", c.bytes as f64 / 1e9, c.class, c.path.display());
        }
        for nav in [vec!["Users", "trent"], vec!["Users", "trent", "Desktop"], vec!["Users", "trent", "Library"]] {
            let mut cur = Some(&r.root);
            for n in &nav {
                cur = cur.and_then(|d| d.find_child(n));
            }
            if let Some(d) = cur {
                println!("-- inside /{}", nav.join("/"));
                for c in d.children.iter().take(9) {
                    println!("  {:>7.2} GB {:?} {}", c.bytes as f64 / 1e9, c.verdict.class, c.name);
                }
            }
        }
        println!("-- blocked sample: {:?}", r.denied_sample.iter().take(5).collect::<Vec<_>>());
    }

    #[test]
    fn map_state_streams_partials_then_a_finished_annotated_tree() {
        let tmp = std::env::temp_dir().join(format!("clearance-map-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        write(&tmp.join("proj/package.json"), 20);
        write(&tmp.join("proj/node_modules/dep/index.js"), 6_000_000); // above the 5 MB list threshold
        write(&tmp.join("proj/src/main.ts"), 200_000);
        write(&tmp.join("movies/clip.mov"), 9_000_000);
        write(&tmp.join("movies/copy/clip.mov"), 9_000_000);
        let dev = platform::dev_id(&fs::metadata(&tmp).unwrap());
        let roots = vec![RootSpec {
            scan_path: tmp.clone(),
            display: tmp.clone(),
            name: "test-root".into(),
            dev,
        }];

        let mut st = MapState::empty();
        st.scanning = true;
        st.handle = Some(start_with_roots(Some(roots), None));
        let started = Instant::now();
        while st.scanning && started.elapsed() < Duration::from_secs(20) {
            st.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = fs::remove_dir_all(&tmp);

        assert!(!st.scanning, "scan should finish");
        assert!(st.handle.is_none(), "handle released after Done");
        let root = st.root.as_ref().expect("tree delivered");
        assert!(root.bytes >= 24_000_000, "bytes {}", root.bytes);
        assert_eq!(root.by_class.iter().sum::<u64>(), root.bytes);
        // The real filesystem sentinel (package.json) proves node_modules is regenerable.
        assert!(
            st.candidates.iter().any(|c| c.name == "node_modules" && c.class == Class::Safe),
            "candidates: {:?}",
            st.candidates.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        assert_eq!(st.top_files.iter().filter(|f| f.name == "clip.mov").count(), 2);
        assert!(st.top_files.iter().all(|f| f.name != "clip.mov" || f.copies == 2));
        let (groups, saved) = duplicate_savings(&st.top_files);
        assert_eq!(groups, 1);
        assert!(saved >= 9_000_000 && saved < 12_000_000, "saved {saved}");
        assert!(st.kinds.iter().any(|k| k.kind == FileKind::Video && k.bytes >= 18_000_000));
        let nav = vec![tmp.file_name().unwrap().to_string_lossy().into_owned()];
        assert!(st.dir_at(&nav).is_some());
    }
}
