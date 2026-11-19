use std::path::PathBuf;
use serde::{Deserialize, Serialize};

/// High-level storage categories surfaced by Cleanse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Category {
    DevArtifacts,
    SystemCaches,
    ToolchainHoards,
    InstallerGhosts,
    OrphanedLeftovers,
    TrashBin,
    Heavyweights,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::DevArtifacts,
        Category::ToolchainHoards,
        Category::SystemCaches,
        Category::InstallerGhosts,
        Category::OrphanedLeftovers,
        Category::TrashBin,
        Category::Heavyweights,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Category::DevArtifacts => "dev-artifacts",
            Category::SystemCaches => "system-caches",
            Category::ToolchainHoards => "toolchain-hoards",
            Category::InstallerGhosts => "installer-ghosts",
            Category::OrphanedLeftovers => "orphaned-leftovers",
            Category::TrashBin => "trash-bin",
            Category::Heavyweights => "heavyweights",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Category::DevArtifacts => "Dormant Build Artifacts",
            Category::SystemCaches => "App & System Caches",
            Category::ToolchainHoards => "SDK & Package Hoards",
            Category::InstallerGhosts => "Forgotten Installers",
            Category::OrphanedLeftovers => "Orphaned App Leftovers",
            Category::TrashBin => "Unemptied Trash",
            Category::Heavyweights => "Large File Radar",
        }
    }

    pub fn short_title(self) -> &'static str {
        match self {
            Category::DevArtifacts => "Dormant Builds",
            Category::SystemCaches => "App Caches",
            Category::ToolchainHoards => "SDK Hoards",
            Category::InstallerGhosts => "Installers",
            Category::OrphanedLeftovers => "App Leftovers",
            Category::TrashBin => "Trash",
            Category::Heavyweights => "Large Files",
        }
    }

    pub fn tagline(self) -> &'static str {
        match self {
            Category::DevArtifacts => "Inactive target/, node_modules/, .next/, and DerivedData verified by project sentinels",
            Category::SystemCaches => "Regenerable ~/Library/Caches and diagnostic logs from non-running apps",
            Category::ToolchainHoards => "Cargo, Homebrew, pnpm, npm, uv, and Xcode simulator download caches",
            Category::InstallerGhosts => "Stale .dmg, .pkg, and .ipsw disk images in Downloads after app install",
            Category::OrphanedLeftovers => "Residual support caches from apps no longer present in /Applications",
            Category::TrashBin => "Items sitting in ~/.Trash still occupying APFS blocks",
            Category::Heavyweights => "Inspect-only >100 MB user files — never auto-selected or batch-cleaned",
        }
    }

    /// OKLCH hue angle for consistent visual coding across treemap, horizon bar, and badges.
    pub fn hue(self) -> f32 {
        match self {
            Category::DevArtifacts => 255.0,      // Cobalt blue
            Category::ToolchainHoards => 195.0,   // Cyan-teal
            Category::SystemCaches => 152.0,      // Emerald
            Category::InstallerGhosts => 72.0,    // Amber-gold
            Category::OrphanedLeftovers => 315.0, // Orchid
            Category::TrashBin => 28.0,           // Coral-orange
            Category::Heavyweights => 280.0,      // Indigo-slate
        }
    }
}

/// Constitutional safety classification for every discovered path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SafetyTier {
    /// 100% regenerable by OS, app, or build tool (`cargo build`, `npm install`, app relaunch).
    SafeRegenerable,
    /// Safe to reclaim after a glance (stale `.dmg` installers, `~/.Trash`, orphaned caches).
    ReviewRecommended,
    /// Personal files, active app caches, or large media — locked out of batch cleaning.
    ProtectedInspectOnly,
}

impl SafetyTier {
    pub fn badge_label(self) -> &'static str {
        match self {
            SafetyTier::SafeRegenerable => "Safe · Regenerable",
            SafetyTier::ReviewRecommended => "Review · Opt-In",
            SafetyTier::ProtectedInspectOnly => "Protected · Inspect Only",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            SafetyTier::SafeRegenerable => "Safe",
            SafetyTier::ReviewRecommended => "Review",
            SafetyTier::ProtectedInspectOnly => "Protected",
        }
    }

    pub fn can_clean(self) -> bool {
        !matches!(self, SafetyTier::ProtectedInspectOnly)
    }
}

/// A single storage finding on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanItem {
    pub id: String,
    pub name: String,
    pub subtitle: String,
    pub path: PathBuf,
    /// Actual physical APFS bytes allocated on disk (`st_blocks * 512`).
    pub bytes: u64,
    /// Logical apparent file size (`st_size`), highlighting sparse files when `logical_bytes > bytes`.
    pub logical_bytes: u64,
    pub category: Category,
    pub safety: SafetyTier,
    pub age_days: u32,
    pub file_count: u64,
    /// Exact command or mechanism that recreates this item if needed (e.g. `cargo build`).
    pub rebuild_hint: Option<String>,
    /// Set if a currently running macOS process owns this cache, preventing live disruption.
    pub in_use_by: Option<String>,
    pub selected: bool,
}

impl ScanItem {
    /// Whether this item is eligible to be swept right now.
    pub fn is_selectable(&self) -> bool {
        self.safety.can_clean() && self.in_use_by.is_none()
    }

    /// Whether this item passes the user's Dormancy Horizon filter (in days).
    pub fn matches_horizon(&self, min_dormant_days: u32) -> bool {
        match self.category {
            Category::DevArtifacts | Category::InstallerGhosts => self.age_days >= min_dormant_days,
            _ => true,
        }
    }
}

/// APFS & volume capacity snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskOverview {
    pub volume_name: String,
    pub mount_point: String,
    pub filesystem: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub purgeable_bytes: u64,
    pub local_snapshots: usize,
}

impl Default for DiskOverview {
    fn default() -> Self {
        Self {
            volume_name: "Macintosh HD".into(),
            mount_point: "/".into(),
            filesystem: "APFS".into(),
            total_bytes: 500_000_000_000,
            free_bytes: 68_400_000_000,
            purgeable_bytes: 18_200_000_000,
            local_snapshots: 2,
        }
    }
}

impl DiskOverview {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    pub fn used_fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes() as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32
    }
}

/// How Cleanse reclaims selected files without risking irreversible mistakes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CleanStrategy {
    /// Instant same-volume move into `~/.clearance-vault/<id>/` with 1-click Undo.
    QuarantineVault,
    /// Move into macOS `~/.Trash` so Finder's Put Back / Empty Trash stays in control.
    MacTrash,
    /// Validate every safety rule and generate a sweep receipt without moving any bytes.
    DryRun,
}

impl CleanStrategy {
    pub const ALL: [CleanStrategy; 3] = [
        CleanStrategy::QuarantineVault,
        CleanStrategy::MacTrash,
        CleanStrategy::DryRun,
    ];

    pub fn action_verb(self) -> &'static str {
        match self {
            CleanStrategy::QuarantineVault => "Hold to Stage Safe Sweep",
            CleanStrategy::MacTrash => "Hold to Move to Trash",
            CleanStrategy::DryRun => "Run Safety Simulation",
        }
    }
}

/// Persisted manifest for a Quarantine Vault sweep so the user can restore anytime.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultManifest {
    pub sweep_id: String,
    pub created_epoch_secs: u64,
    pub label: String,
    pub total_bytes: u64,
    pub entries: Vec<VaultMovedEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultMovedEntry {
    pub name: String,
    pub original_path: PathBuf,
    pub vaulted_path: PathBuf,
    pub bytes: u64,
    pub category: Category,
}

/// Result of executing a sweep or dry-run simulation.
#[derive(Clone, Debug)]
pub struct SweepOutcome {
    pub strategy: CleanStrategy,
    pub sweep_id: Option<String>,
    pub reclaimed_bytes: u64,
    pub moved_ids: Vec<String>,
    pub skipped_reasons: Vec<String>,
}

/// Dataset selector supporting both real macOS scanning and `break-ui` stress testing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatasetMode {
    LiveMac,
    Demo,
    WorstCase,
    Empty,
    SingleItem,
    Massive5k,
}

impl DatasetMode {
    pub const ALL: [DatasetMode; 6] = [
        DatasetMode::LiveMac,
        DatasetMode::Demo,
        DatasetMode::WorstCase,
        DatasetMode::Empty,
        DatasetMode::SingleItem,
        DatasetMode::Massive5k,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DatasetMode::LiveMac => if crate::platform::IS_WINDOWS { "Live PC" } else { "Live Mac" },
            DatasetMode::Demo => "Demo",
            DatasetMode::WorstCase => "Worst Case",
            DatasetMode::Empty => "Empty",
            DatasetMode::SingleItem => "1 Item",
            DatasetMode::Massive5k => "5,000 Rows",
        }
    }
}

/// Prototype layout variants for the Overview analyzer (`prototype` skill).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeroVariant {
    /// Proportional squarified Spatial Bento Map + Horizon bar (Default).
    SpatialBento,
    /// Quiet, ultra-dense linear Horizon Ledger for daily power users.
    HorizonLedger,
    /// Vector Runway Arc Gauge + Dormancy Project Timeline.
    RunwayRadar,
}

impl HeroVariant {
    pub const ALL: [HeroVariant; 3] = [
        HeroVariant::SpatialBento,
        HeroVariant::HorizonLedger,
        HeroVariant::RunwayRadar,
    ];
}

/// Format byte counts with clean decimal units (macOS Finder uses base-1000 KB/MB/GB/TB).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    const TB: f64 = 1_000_000_000_000.0;

    let b = bytes as f64;
    if bytes == 0 {
        "0 B".into()
    } else if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        let mb = b / MB;
        if mb >= 100.0 {
            format!("{:.0} MB", mb)
        } else {
            format!("{:.1} MB", mb)
        }
    } else if b < TB {
        let gb = b / GB;
        if gb >= 100.0 {
            format!("{:.1} GB", gb)
        } else {
            format!("{:.2} GB", gb)
        }
    } else {
        format!("{:.2} TB", b / TB)
    }
}

/// Grammatical English pluralization (`break-ui` rule: never render `"1 items"`).
pub fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{} {plural}", format_count(count))
    }
}

/// Locale-style thousands separator for integer counts (`1,284`, never `1284`).
pub fn format_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

pub fn format_age(days: u32) -> String {
    if days == 0 {
        "Today".into()
    } else if days == 1 {
        "1d ago".into()
    } else if days < 30 {
        format!("{days}d ago")
    } else if days < 365 {
        let months = days / 30;
        if months == 1 {
            "1mo ago".into()
        } else {
            format!("{months}mo ago")
        }
    } else {
        let years = days / 365;
        if years == 1 {
            "1yr ago".into()
        } else {
            format!("{years}yr ago")
        }
    }
}
