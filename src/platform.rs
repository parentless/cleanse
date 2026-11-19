//! Everything that differs between macOS and Windows lives here (`platform.rs`).
//!
//! The scanners and the safety layer call these helpers instead of touching `std::os::unix`,
//! `libc` or `cmd.exe` directly, so one code base builds and behaves correctly on both.

use std::collections::HashSet;
use std::fs::Metadata;
use std::path::{Path, PathBuf};

/// `true` on Windows builds. Rules that only apply there are gated on this at runtime so they
/// can still be unit-tested on any machine.
pub const IS_WINDOWS: bool = cfg!(windows);

/// What to tell someone whose OS blocked a folder.
pub const ACCESS_FIX: &str = if IS_WINDOWS {
    "Run Cleanse as administrator to see inside."
} else {
    "Grant Full Disk Access to see inside."
};

/// What the machine is called in the UI.
pub const DEVICE: &str = if IS_WINDOWS { "PC" } else { "Mac" };
/// The operating system, as users call it.
pub const OS_NAME: &str = if IS_WINDOWS { "Windows" } else { "macOS" };
/// The system file browser.
pub const FILE_MANAGER: &str = if IS_WINDOWS { "File Explorer" } else { "Finder" };
/// Where deleted-but-recoverable files sit.
pub const TRASH_NAME: &str = if IS_WINDOWS { "Recycle Bin" } else { "Trash" };

/// Name of the top of the disk tree.
pub const ROOT_NAME: &str = if IS_WINDOWS { "This PC" } else { "Macintosh HD" };
/// Path of the (virtual) top of the disk tree.
pub const ROOT_PATH: &str = if IS_WINDOWS { "" } else { "/" };

/// Turn the macOS key glyphs used in labels into what the keyboard actually says.
pub fn kbd(label: &str) -> String {
    if IS_WINDOWS {
        label.replace('⌘', "Ctrl+").replace('⇧', "Shift+").replace('⌥', "Alt+")
    } else {
        label.to_owned()
    }
}

/// The current user's home folder.
pub fn home_dir() -> PathBuf {
    if IS_WINDOWS {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"))
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Users/Shared"))
    }
}

// ---------------------------------------------------------------------------
// File metadata
// ---------------------------------------------------------------------------

/// Bytes actually occupying the disk (not the apparent size).
#[cfg(unix)]
pub fn phys_bytes(m: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.blocks().saturating_mul(512)
}

/// Windows has no cheap allocated-size call, so use the length rounded up to a 4 KB cluster.
/// Cloud placeholders (OneDrive "online-only" files) occupy no disk at all.
#[cfg(windows)]
pub fn phys_bytes(m: &Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    if m.file_attributes() & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0 {
        return 0;
    }
    let len = m.len();
    if len == 0 { 0 } else { len.div_ceil(4096) * 4096 }
}

/// Last-modified time, seconds since the Unix epoch.
#[cfg(unix)]
pub fn mtime_secs(m: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.mtime().max(0) as u64
}

#[cfg(windows)]
pub fn mtime_secs(m: &Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Device id, so a scan can refuse to cross onto another volume. Windows scans one drive at a
/// time and never follows junctions, so a constant is correct there.
#[cfg(unix)]
pub fn dev_id(m: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.dev()
}

#[cfg(windows)]
pub fn dev_id(_m: &Metadata) -> u64 {
    0
}

/// `(device, inode)` for files with more than one hard link, so each is counted once.
#[cfg(unix)]
pub fn hard_link_key(m: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (m.nlink() > 1).then(|| (m.dev(), m.ino()))
}

/// Windows file ids are not available on stable Rust, so hard links (mostly `WinSxS`, which is
/// system-owned anyway) are counted at each path.
#[cfg(windows)]
pub fn hard_link_key(_m: &Metadata) -> Option<(u64, u64)> {
    None
}

/// `true` if the OS refused access to a directory (not "does not exist").
pub fn is_access_denied(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
        || e.raw_os_error() == Some(if IS_WINDOWS { 5 } else { 1 })
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

/// `(total_bytes, bytes_available_to_this_user)` for the volume holding `path`.
#[cfg(unix)]
pub fn capacity(path: &Path) -> Option<(u64, u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 || stat.f_blocks == 0 {
        return None;
    }
    let bs = if stat.f_frsize > 0 { stat.f_frsize as u64 } else { stat.f_bsize as u64 };
    Some((
        (stat.f_blocks as u64).saturating_mul(bs),
        (stat.f_bavail as u64).saturating_mul(bs),
        (stat.f_bfree as u64).saturating_mul(bs),
    ))
}

#[cfg(windows)]
unsafe extern "system" {
    fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
}

/// `(total, available_to_user, free_overall)`.
#[cfg(windows)]
pub fn capacity(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0 && total > 0).then_some((total, avail, free))
}

/// The root of the volume the OS lives on (`C:\` on Windows, `/System/Volumes/Data` on a Mac).
pub fn system_volume_root() -> PathBuf {
    if IS_WINDOWS {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        PathBuf::from(format!("{drive}\\"))
    } else {
        PathBuf::from("/System/Volumes/Data")
    }
}

/// Every fixed (non-removable, non-network) drive root on Windows, e.g. `C:\`, `D:\`.
#[cfg(windows)]
pub fn fixed_drives() -> Vec<PathBuf> {
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| {
            let root = format!("{}:\\", (b'A' + i as u8) as char);
            let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            (unsafe { GetDriveTypeW(wide.as_ptr()) } == DRIVE_FIXED).then(|| PathBuf::from(root))
        })
        .collect()
}

#[cfg(not(windows))]
pub fn fixed_drives() -> Vec<PathBuf> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// Processes and shelling out
// ---------------------------------------------------------------------------

/// Lower-case names of running programs (`spotify`, `code`), so a cache owned by a running app
/// is never touched.
pub fn running_process_names() -> HashSet<String> {
    let mut set = HashSet::new();
    let out = if IS_WINDOWS {
        std::process::Command::new("tasklist").args(["/FO", "CSV", "/NH"]).output()
    } else {
        std::process::Command::new("ps").args(["-axo", "comm="]).output()
    };
    let Ok(out) = out else {
        return set;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let name = if IS_WINDOWS {
            // "Image Name","PID",... — the first quoted field.
            line.trim().trim_start_matches('"').split("\",\"").next().unwrap_or("").to_owned()
        } else {
            let t = line.trim();
            Path::new(t).file_name().and_then(|s| s.to_str()).unwrap_or(t).to_owned()
        };
        let mut base = name.to_ascii_lowercase();
        if let Some(stripped) = base.strip_suffix(".exe") {
            base = stripped.to_owned();
        }
        if base.len() >= 3 {
            set.insert(base);
        }
    }
    set
}

/// Show `path` selected in the system file browser. No-op in headless runs.
pub fn reveal(path: &Path) {
    if rune::is_headless() || !path.exists() {
        return;
    }
    let _ = if IS_WINDOWS {
        std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn()
    } else {
        std::process::Command::new("open").arg("-R").arg(path).spawn()
    };
}

#[cfg(windows)]
#[repr(C)]
struct ShFileOpW {
    hwnd: *mut std::ffi::c_void,
    func: u32,
    from: *const u16,
    to: *const u16,
    flags: u16,
    aborted: i32,
    mappings: *mut std::ffi::c_void,
    title: *const u16,
}

#[cfg(windows)]
#[link(name = "shell32")]
unsafe extern "system" {
    fn SHFileOperationW(op: *mut ShFileOpW) -> i32;
}

/// Send a file or folder to the Windows Recycle Bin (so "Restore" works). Other platforms have
/// their own Trash handling in `cleaner.rs`.
#[cfg(windows)]
pub fn send_to_recycle_bin(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const FO_DELETE: u32 = 3;
    const FOF_SILENT: u16 = 0x0004;
    const FOF_NOCONFIRMATION: u16 = 0x0010;
    const FOF_ALLOWUNDO: u16 = 0x0040;
    const FOF_NOERRORUI: u16 = 0x0400;
    // `pFrom` is a list of paths ending with a double NUL.
    let from: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    let mut op = ShFileOpW {
        hwnd: std::ptr::null_mut(),
        func: FO_DELETE,
        from: from.as_ptr(),
        to: std::ptr::null(),
        flags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT,
        aborted: 0,
        mappings: std::ptr::null_mut(),
        title: std::ptr::null(),
    };
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc == 0 && op.aborted == 0 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("Recycle Bin refused the item (code {rc})")))
    }
}

#[cfg(not(windows))]
pub fn send_to_recycle_bin(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

/// Number of local Time Machine snapshots (always 0 off macOS).
pub fn local_snapshot_names(volume: &str) -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let Ok(out) = std::process::Command::new("tmutil").args(["listlocalsnapshots", volume]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("com.apple."))
        .map(str::to_owned)
        .collect()
}

/// Lower-cased copy of a path on Windows (whose file names ignore case); unchanged elsewhere.
pub fn fold(p: PathBuf) -> PathBuf {
    if IS_WINDOWS {
        PathBuf::from(p.to_string_lossy().to_lowercase())
    } else {
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kbd_labels_follow_the_platform() {
        let k = kbd("⇧⌘L");
        if IS_WINDOWS {
            assert_eq!(k, "Shift+Ctrl+L");
        } else {
            assert_eq!(k, "⇧⌘L");
        }
    }

    #[test]
    fn home_dir_is_absolute_and_capacity_reads_the_system_volume() {
        assert!(home_dir().is_absolute());
        let (total, avail, _) = capacity(&system_volume_root()).or_else(|| capacity(Path::new("/"))).expect("volume");
        assert!(total > 0 && avail <= total);
    }

    #[test]
    fn running_processes_include_this_test_runner() {
        let procs = running_process_names();
        assert!(!procs.is_empty());
    }

    #[test]
    fn denied_errors_are_recognised() {
        let e = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(is_access_denied(&e));
        assert!(!is_access_denied(&std::io::Error::from(std::io::ErrorKind::NotFound)));
    }
}
