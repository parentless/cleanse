pub mod charts;
pub mod controls;
pub mod diskmap_view;
pub mod theme;

use std::borrow::Cow;
use std::time::Duration;

use rune::{
    Align, Cx, El, Key, KeyEvent, ScrollSpec, Spring, TextEdit, Tx, Waker, col, el, icon, oklch,
    oklcha, row, spacer, text,
};
use rune_kit::shell::{
    self, PaneMotion, PaneState, Sidebar, SidebarPeek, SidebarRow, SidebarSection, SidebarToggle,
    SplitView, Toolbar,
};
use rune_kit::{
    Button, ButtonVariant, Cell, Checkbox, ControlSize, IconButton, IconButtonVariant, Kit,
    RollingText, SegmentedControl, SortDir, Switch, Table, TableColumn, TableState, TableStyle,
    TextInput, Toast, ToastEffect, ToastEvent, ToastRegion, ToastStack, glyphs,
};

use crate::cleaner::{
    execute_sweep, load_vault_manifests, move_vault_sweep_to_trash, restore_vault_sweep,
    reveal_in_finder,
};
use crate::diskmap::{MapState, demo_map};
use crate::fixtures::dataset_for;
use crate::platform;
use crate::model::{
    Category, CleanStrategy, DatasetMode, DiskOverview, HeroVariant, SafetyTier, ScanItem,
    VaultManifest, format_age, format_bytes, format_count, pluralize,
};
use crate::scanner::{ScanHandle, ScanMessage, query_disk_overview, start_live_scan};
use crate::ui::charts::{
    horizon_ledger_view, runway_radar_view, spatial_bento_map, storage_horizon_bar,
    summarize_categories,
};
use crate::ui::diskmap_view::{MapUi, Sel, build_map_inspector, build_map_view};
use crate::ui::controls::{
    HoldSweepState, HorizonSliderState, dormancy_slider, hold_to_sweep_button, safety_pill,
};
use crate::ui::theme::{
    ICON_GRID, ICON_LIST, ICON_MAP, ICON_MOON, ICON_REFRESH, ICON_SHIELD, ICON_SUN, ICON_UNDO, ICON_VAULT,
    UI_SPRING, category_accent, category_border, category_icon, category_surface, clearance_kit,
    hero_metric_font, tabular_small, tabular_small_medium, tabular_ui, tabular_ui_medium,
};

/// Active main navigation surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavRoute {
    Overview,
    DiskMap,
    AllFindings,
    CategoryFocus(Category),
    QuarantineVault,
    SafetyConstitution,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToastPayload {
    pub title: String,
    pub description: String,
    pub undo_sweep_id: Option<String>,
}

/// Synthetic undo buffer so Demo & WorstCase sweeps also support 1-click Undo without touching disk!
#[derive(Clone, Debug)]
pub struct SyntheticVaultSweep {
    pub manifest: VaultManifest,
    pub removed_items: Vec<ScanItem>,
}

#[derive(Clone, Debug)]
struct PaletteCommand {
    id: String,
    title: String,
    subtitle: String,
    shortcut: Option<&'static str>,
    action: PaletteAction,
}

#[derive(Clone, Debug)]
enum PaletteAction {
    Navigate(NavRoute),
    ScanLiveMac,
    SelectSafeOnly,
    ClearSelection,
    UndoLatestSweep,
    SetStrategy(CleanStrategy),
    SetHero(HeroVariant),
    SetDataset(DatasetMode),
    ToggleDark,
    ToggleDevHarness,
}

pub struct ClearanceApp {
    pub route: NavRoute,
    pub dataset: DatasetMode,
    pub hero_variant: HeroVariant,
    pub strategy: CleanStrategy,
    pub dark_override: Option<bool>,
    pub reduced_motion_override: Option<bool>,
    pub dev_harness_open: bool,

    pub disk: DiskOverview,
    pub items: Vec<ScanItem>,
    pub visible_indices: Vec<usize>,
    pub sort_col: String,
    pub sort_dir: SortDir,

    pub search: TextEdit,
    pub horizon: HorizonSliderState,
    pub hold_sweep: HoldSweepState,

    pub sidebar: PaneState,
    pub inspector: PaneState,
    pub inspected_id: Option<String>,
    pub table: TableState,

    pub map: MapState,
    pub map_ui: MapUi,

    pub scanning: bool,
    pub scan_phase: String,
    pub scanned_dirs: usize,
    pub last_scan_ms: Option<u64>,
    pub scan_handle: Option<ScanHandle>,
    pub waker: Option<Waker>,

    pub real_vault: Vec<VaultManifest>,
    pub synthetic_vault: Vec<SyntheticVaultSweep>,
    pub toasts: ToastStack<ToastPayload>,

    pub palette_open: bool,
    pub palette_query: TextEdit,
    pub palette_cursor: usize,
}

impl ClearanceApp {
    pub fn init() -> Self {
        let args = rune::extra_args();
        let force_live = args.iter().any(|a| a == "live");
        let force_demo = args.iter().any(|a| a == "demo");
        let force_worst = args.iter().any(|a| a == "worst-case");

        let dataset = if force_worst {
            DatasetMode::WorstCase
        } else if force_demo || (rune::is_headless() && !force_live) {
            DatasetMode::Demo
        } else {
            DatasetMode::LiveMac
        };

        let (disk, items) = if dataset == DatasetMode::LiveMac {
            (query_disk_overview(), Vec::new())
        } else {
            dataset_for(dataset)
        };

        let mut table = TableState::new().key("findings-table");
        table.set_sort("size", SortDir::Descending);

        let mut app = Self {
            route: NavRoute::Overview,
            dataset,
            hero_variant: HeroVariant::SpatialBento,
            strategy: CleanStrategy::QuarantineVault,
            dark_override: None,
            reduced_motion_override: None,
            dev_harness_open: false,

            disk,
            items,
            visible_indices: Vec::new(),
            sort_col: "size".into(),
            sort_dir: SortDir::Descending,

            search: TextEdit::new(""),
            horizon: HorizonSliderState {
                days: 14,
                ..Default::default()
            },
            hold_sweep: HoldSweepState::default(),

            sidebar: PaneState::new(244.0).range(196.0, 340.0),
            inspector: PaneState::new(308.0).range(260.0, 420.0),
            inspected_id: None,
            table,

            map: if dataset == DatasetMode::LiveMac {
                MapState::empty()
            } else {
                demo_map()
            },
            map_ui: MapUi::new(),

            scanning: false,
            scan_phase: "Ready".into(),
            scanned_dirs: 0,
            last_scan_ms: Some(38),
            scan_handle: None,
            waker: None,

            real_vault: if rune::is_headless() {
                Vec::new()
            } else {
                load_vault_manifests()
            },
            synthetic_vault: Vec::new(),
            toasts: ToastStack::default(),

            palette_open: false,
            palette_query: TextEdit::new(""),
            palette_cursor: 0,
        };
        app.recompute_visible();
        if let Some(&first_idx) = app.visible_indices.first() {
            let first_id = app.items[first_idx].id.clone();
            app.inspected_id = Some(first_id.clone());
            app.table.select(first_id);
        }
        app
    }

    pub fn setup(&mut self, waker: Waker) {
        self.waker = Some(waker);
        if self.dataset == DatasetMode::LiveMac && !rune::is_headless() {
            self.begin_live_scan();
        }
    }

    pub fn begin_live_scan(&mut self) {
        if let Some(prev) = self.scan_handle.take() {
            prev.cancel();
        }
        self.dataset = DatasetMode::LiveMac;
        self.items.clear();
        self.visible_indices.clear();
        self.scanning = true;
        self.scan_phase = "Reading volume metrics…".into();
        self.scanned_dirs = 0;
        self.last_scan_ms = None;
        self.real_vault = load_vault_manifests();
        self.scan_handle = Some(start_live_scan(self.waker.clone()));
        self.map_ui.nav.clear();
        self.map_ui.selected = None;
        self.map_ui.filter = None;
        self.map.start(self.waker.clone());
    }

    pub fn switch_dataset(&mut self, mode: DatasetMode) {
        if let Some(prev) = self.scan_handle.take() {
            prev.cancel();
        }
        self.scanning = false;
        self.dataset = mode;
        if mode == DatasetMode::LiveMac {
            self.begin_live_scan();
            return;
        }
        self.map.stop();
        self.map = demo_map();
        self.map_ui = MapUi::new();
        let (disk, items) = dataset_for(mode);
        self.disk = disk;
        self.items = items;
        self.last_scan_ms = Some(19);
        self.recompute_visible();
        self.inspected_id = self
            .visible_indices
            .first()
            .map(|&i| self.items[i].id.clone());
        if let Some(id) = self.inspected_id.clone() {
            self.table.select(id);
        }
    }

    pub fn recompute_visible(&mut self) {
        let query = self.search.text.trim().to_ascii_lowercase();
        let cat_filter = match self.route {
            NavRoute::CategoryFocus(cat) => Some(cat),
            _ => None,
        };
        let min_days = self.horizon.days;

        let mut indices: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                if let Some(c) = cat_filter
                    && it.category != c
                {
                    return false;
                }
                if !it.matches_horizon(min_days) {
                    return false;
                }
                if !query.is_empty() {
                    let path_s = it.path.to_string_lossy().to_ascii_lowercase();
                    let hit = it.name.to_ascii_lowercase().contains(&query)
                        || it.subtitle.to_ascii_lowercase().contains(&query)
                        || path_s.contains(&query)
                        || it.category.title().to_ascii_lowercase().contains(&query);
                    if !hit {
                        return false;
                    }
                }
                true
            })
            .map(|(idx, _)| idx)
            .collect();

        let col = self.sort_col.as_str();
        let dir = self.sort_dir;
        indices.sort_by(|&a, &b| {
            let ia = &self.items[a];
            let ib = &self.items[b];
            let ord = match col {
                "name" => ia
                    .name
                    .to_ascii_lowercase()
                    .cmp(&ib.name.to_ascii_lowercase()),
                "category" => ia.category.title().cmp(ib.category.title()),
                "safety" => ia.safety.cmp(&ib.safety),
                "age" => ia.age_days.cmp(&ib.age_days),
                _ => ia.bytes.cmp(&ib.bytes),
            };
            dir.apply(ord)
        });

        self.visible_indices = indices;
        if let Some(cur) = &self.inspected_id
            && !self.items.iter().any(|x| &x.id == cur)
        {
            self.inspected_id = self
                .visible_indices
                .first()
                .map(|&i| self.items[i].id.clone());
        }
    }

    /// Total selected & eligible bytes within the active Dormancy Horizon.
    pub fn selected_reclaimable_bytes(&self) -> (usize, u64) {
        let mut count = 0usize;
        let mut bytes = 0u64;
        for it in &self.items {
            if it.selected && it.is_selectable() && it.matches_horizon(self.horizon.days) {
                count += 1;
                bytes = bytes.saturating_add(it.bytes);
            }
        }
        (count, bytes)
    }

    pub fn reviewable_unselected_bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|it| {
                (!it.selected || !it.is_selectable()) && it.matches_horizon(self.horizon.days)
            })
            .map(|it| it.bytes)
            .sum()
    }

    pub fn select_safe_regenerable_only(&mut self) {
        for it in &mut self.items {
            it.selected = it.safety == SafetyTier::SafeRegenerable
                && it.in_use_by.is_none()
                && it.matches_horizon(self.horizon.days);
        }
    }

    pub fn clear_all_selections(&mut self) {
        for it in &mut self.items {
            it.selected = false;
        }
    }

    pub fn toggle_item_by_id(&mut self, id: &str) {
        if let Some(it) = self.items.iter_mut().find(|x| x.id == id)
            && it.is_selectable()
        {
            it.selected = !it.selected;
        }
    }

    pub fn push_toast(&mut self, key: impl Into<String>, payload: ToastPayload, now: f64, tx: &mut Tx) {
        let fx = self.toasts.step(ToastEvent::Shown {
            key: key.into(),
            options: payload,
            secs: Some(6.0),
            now,
        });
        for eff in fx {
            if let ToastEffect::WakeAt(due) = eff {
                tx.wake_at(due);
            }
        }
    }

    pub fn trigger_sweep(&mut self, tx: &mut Tx) {
        let now = tx.clock();
        self.hold_sweep.holding = false;
        self.hold_sweep.progress = 0.0;
        self.hold_sweep.fired_this_press = true;

        // Snapshot eligible items respecting current Dormancy Horizon
        let min_days = self.horizon.days;
        let staged_candidates: Vec<ScanItem> = self
            .items
            .iter()
            .filter(|it| it.matches_horizon(min_days))
            .cloned()
            .collect();

        let outcome = execute_sweep(&staged_candidates, self.strategy, self.dataset);
        if outcome.moved_ids.is_empty() {
            let reason = outcome
                .skipped_reasons
                .first()
                .cloned()
                .unwrap_or_else(|| "No eligible items were selected.".into());
            self.push_toast(
                "sweep-empty",
                ToastPayload {
                    title: "Nothing Swept".into(),
                    description: reason,
                    undo_sweep_id: None,
                },
                now,
                tx,
            );
            return;
        }

        if outcome.strategy == CleanStrategy::DryRun {
            let desc = format!(
                "Verified {} ({}) passed all 5 constitutional safety barriers. Zero files moved.",
                pluralize(outcome.moved_ids.len(), "item", "items"),
                format_bytes(outcome.reclaimed_bytes)
            );
            self.push_toast(
                "dry-run-ok",
                ToastPayload {
                    title: "Dry-Run Simulation Passed".into(),
                    description: desc,
                    undo_sweep_id: None,
                },
                now,
                tx,
            );
            return;
        }

        // Remove swept items from live table and update free space readout
        let moved_set: std::collections::HashSet<&str> =
            outcome.moved_ids.iter().map(String::as_str).collect();
        let mut removed_items = Vec::new();
        let mut kept_items = Vec::with_capacity(self.items.len());
        for it in self.items.drain(..) {
            if moved_set.contains(it.id.as_str()) {
                removed_items.push(it);
            } else {
                kept_items.push(it);
            }
        }
        self.items = kept_items;
        self.disk.free_bytes = self
            .disk
            .free_bytes
            .saturating_add(outcome.reclaimed_bytes)
            .min(self.disk.total_bytes);

        if self.dataset == DatasetMode::LiveMac {
            self.real_vault = load_vault_manifests();
        } else if let Some(sid) = &outcome.sweep_id {
            let entries = removed_items
                .iter()
                .map(|it| crate::model::VaultMovedEntry {
                    name: it.name.clone(),
                    original_path: it.path.clone(),
                    vaulted_path: std::path::PathBuf::from(format!(
                        "/Users/alex/.clearance-vault/{sid}/{}",
                        it.id
                    )),
                    bytes: it.bytes,
                    category: it.category,
                })
                .collect();
            self.synthetic_vault.insert(
                0,
                SyntheticVaultSweep {
                    manifest: VaultManifest {
                        sweep_id: sid.clone(),
                        created_epoch_secs: 1_775_000_000,
                        label: format!(
                            "Staged {} ({})",
                            pluralize(removed_items.len(), "item", "items"),
                            format_bytes(outcome.reclaimed_bytes)
                        ),
                        total_bytes: outcome.reclaimed_bytes,
                        entries,
                    },
                    removed_items,
                },
            );
        }

        self.recompute_visible();
        tx.animated();
        tx.motion(UI_SPRING);

        let title = match outcome.strategy {
            CleanStrategy::QuarantineVault => {
                format!("Staged {} in Quarantine Vault", format_bytes(outcome.reclaimed_bytes))
            }
            CleanStrategy::MacTrash => {
                format!("Moved {} to the {}", format_bytes(outcome.reclaimed_bytes), platform::TRASH_NAME)
            }
            CleanStrategy::DryRun => unreachable!(),
        };
        let description = format!(
            "{} safely isolated · Zero system or personal files touched.",
            pluralize(outcome.moved_ids.len(), "item", "items")
        );
        self.push_toast(
            format!("sweep-done-{}", outcome.sweep_id.as_deref().unwrap_or("trash")),
            ToastPayload {
                title,
                description,
                undo_sweep_id: outcome.sweep_id,
            },
            now,
            tx,
        );
    }

    pub fn undo_sweep(&mut self, sweep_id: &str, tx: &mut Tx) {
        let now = tx.clock();
        if let Some(pos) = self
            .synthetic_vault
            .iter()
            .position(|sv| sv.manifest.sweep_id == sweep_id)
        {
            let sv = self.synthetic_vault.remove(pos);
            let count = sv.removed_items.len();
            let bytes = sv.manifest.total_bytes;
            self.disk.free_bytes = self.disk.free_bytes.saturating_sub(bytes);
            self.items.extend(sv.removed_items);
            self.recompute_visible();
            tx.animated();
            self.push_toast(
                format!("undo-{sweep_id}"),
                ToastPayload {
                    title: "Sweep Restored".into(),
                    description: format!(
                        "Returned {} ({}) to original paths.",
                        pluralize(count, "item", "items"),
                        format_bytes(bytes)
                    ),
                    undo_sweep_id: None,
                },
                now,
                tx,
            );
            return;
        }

        match restore_vault_sweep(sweep_id) {
            Ok((count, bytes)) => {
                self.real_vault = load_vault_manifests();
                if self.dataset == DatasetMode::LiveMac {
                    self.begin_live_scan();
                }
                self.push_toast(
                    format!("undo-{sweep_id}"),
                    ToastPayload {
                        title: "Vault Sweep Restored".into(),
                        description: format!(
                            "Restored {} ({}) to exact original locations.",
                            pluralize(count, "item", "items"),
                            format_bytes(bytes)
                        ),
                        undo_sweep_id: None,
                    },
                    now,
                    tx,
                );
            }
            Err(err) => {
                self.push_toast(
                    "undo-err",
                    ToastPayload {
                        title: "Could Not Restore Sweep".into(),
                        description: err,
                        undo_sweep_id: None,
                    },
                    now,
                    tx,
                );
            }
        }
    }

    pub fn vault_count(&self) -> usize {
        if self.dataset == DatasetMode::LiveMac {
            self.real_vault.len()
        } else {
            self.synthetic_vault.len()
        }
    }

    fn filtered_palette_commands(&self) -> Vec<PaletteCommand> {
        let mut cmds = vec![
            PaletteCommand {
                id: "cmd-overview".into(),
                title: "Go to Smart Overview".into(),
                subtitle: "Storage horizon bar + spatial breakdown".into(),
                shortcut: Some("⌘1"),
                action: PaletteAction::Navigate(NavRoute::Overview),
            },
            PaletteCommand {
                id: "cmd-diskmap".into(),
                title: "Go to Disk Map".into(),
                subtitle: format!("Every folder on this {}, sized, with what can be deleted", platform::DEVICE),
                shortcut: Some("⌘2"),
                action: PaletteAction::Navigate(NavRoute::DiskMap),
            },
            PaletteCommand {
                id: "cmd-all".into(),
                title: "Go to All Findings Table".into(),
                subtitle: "Virtualized sortable list of every storage item".into(),
                shortcut: Some("⌘3"),
                action: PaletteAction::Navigate(NavRoute::AllFindings),
            },
            PaletteCommand {
                id: "cmd-vault".into(),
                title: "Open Quarantine Vault (Undo Sweeps)".into(),
                subtitle: "Inspect or restore staged sweeps with 1 click".into(),
                shortcut: Some("⌘4"),
                action: PaletteAction::Navigate(NavRoute::QuarantineVault),
            },
            PaletteCommand {
                id: "cmd-safety".into(),
                title: "Inspect 5-Layer Safety Constitution".into(),
                subtitle: "See every protected path and sentinel rule".into(),
                shortcut: Some("⌘5"),
                action: PaletteAction::Navigate(NavRoute::SafetyConstitution),
            },
            PaletteCommand {
                id: "cmd-scan-live".into(),
                title: format!("Scan This {} Now", platform::DEVICE),
                subtitle: "Run a parallel, non-blocking scan of caches, toolchains, and workspaces".into(),
                shortcut: Some("⌘R"),
                action: PaletteAction::ScanLiveMac,
            },
            PaletteCommand {
                id: "cmd-safe-only".into(),
                title: "Select Safe · Regenerable Items Only".into(),
                subtitle: "Selects 100% rebuildable caches & dormant artifacts; deselects opt-in items".into(),
                shortcut: None,
                action: PaletteAction::SelectSafeOnly,
            },
            PaletteCommand {
                id: "cmd-clear-sel".into(),
                title: "Deselect All Items".into(),
                subtitle: "Clear current sweep staging selection".into(),
                shortcut: None,
                action: PaletteAction::ClearSelection,
            },
            PaletteCommand {
                id: "cmd-undo".into(),
                title: "Undo & Restore Latest Quarantine Sweep".into(),
                subtitle: "Put every file from the most recent Vault sweep back in place".into(),
                shortcut: Some("⌘Z"),
                action: PaletteAction::UndoLatestSweep,
            },
            PaletteCommand {
                id: "cmd-dry-run".into(),
                title: "Switch Sweep Mode → Dry-Run Simulation".into(),
                subtitle: "Verify safety proofs without moving any files".into(),
                shortcut: None,
                action: PaletteAction::SetStrategy(CleanStrategy::DryRun),
            },
            PaletteCommand {
                id: "cmd-vault-mode".into(),
                title: "Switch Sweep Mode → Quarantine Vault (Undoable)".into(),
                subtitle: "Atomic move to ~/.clearance-vault with 1-click Undo".into(),
                shortcut: None,
                action: PaletteAction::SetStrategy(CleanStrategy::QuarantineVault),
            },
            PaletteCommand {
                id: "cmd-proto-bento".into(),
                title: "Analyzer View → Spatial Bento Map".into(),
                subtitle: "Proportional squarified tiles with shared-element zoom".into(),
                shortcut: None,
                action: PaletteAction::SetHero(HeroVariant::SpatialBento),
            },
            PaletteCommand {
                id: "cmd-proto-ledger".into(),
                title: "Analyzer View → Quiet Horizon Ledger".into(),
                subtitle: "High-density linear breakdown for keyboard workflows".into(),
                shortcut: None,
                action: PaletteAction::SetHero(HeroVariant::HorizonLedger),
            },
            PaletteCommand {
                id: "cmd-proto-radar".into(),
                title: "Analyzer View → Runway Radar & Dormancy Timeline".into(),
                subtitle: "Vector 240° storage gauge + project age histogram".into(),
                shortcut: None,
                action: PaletteAction::SetHero(HeroVariant::RunwayRadar),
            },
            PaletteCommand {
                id: "cmd-theme".into(),
                title: "Toggle Dark / Light Appearance".into(),
                subtitle: "Preview OKLCH color tokens in light or dark mode".into(),
                shortcut: Some("⇧⌘L"),
                action: PaletteAction::ToggleDark,
            },
            PaletteCommand {
                id: "cmd-harness".into(),
                title: "Toggle Prototype & Stress-Test Harness".into(),
                subtitle: "Switch between Live Mac, Demo, Worst Case, Empty, 1 Item, and 5,000 Rows".into(),
                shortcut: Some("⌘D"),
                action: PaletteAction::ToggleDevHarness,
            },
            PaletteCommand {
                id: "cmd-worst".into(),
                title: "Load Worst-Case Stress Dataset (break-ui)".into(),
                subtitle: "240-char paths, CJK/Polish/Emoji, 1 TB outliers, sparse files".into(),
                shortcut: None,
                action: PaletteAction::SetDataset(DatasetMode::WorstCase),
            },
        ];

        for cat in Category::ALL {
            cmds.push(PaletteCommand {
                id: format!("cmd-cat-{}", cat.id()),
                title: format!("Focus Category: {}", cat.title()),
                subtitle: cat.tagline().into(),
                shortcut: None,
                action: PaletteAction::Navigate(NavRoute::CategoryFocus(cat)),
            });
        }

        let q = self.palette_query.text.trim().to_ascii_lowercase();
        if q.is_empty() {
            return cmds;
        }
        cmds.into_iter()
            .filter(|c| {
                c.title.to_ascii_lowercase().contains(&q)
                    || c.subtitle.to_ascii_lowercase().contains(&q)
            })
            .collect()
    }

    fn execute_palette_action(&mut self, action: PaletteAction, tx: &mut Tx) {
        // Command palette actions execute at 0ms (`remove-animations` / `emil-design-eng`)
        self.palette_open = false;
        tx.instant();

        match action {
            PaletteAction::Navigate(r) => {
                self.route = r;
                self.recompute_visible();
            }
            PaletteAction::ScanLiveMac => self.begin_live_scan(),
            PaletteAction::SelectSafeOnly => self.select_safe_regenerable_only(),
            PaletteAction::ClearSelection => self.clear_all_selections(),
            PaletteAction::UndoLatestSweep => {
                let latest = self
                    .synthetic_vault
                    .first()
                    .map(|s| s.manifest.sweep_id.clone())
                    .or_else(|| self.real_vault.first().map(|m| m.sweep_id.clone()));
                if let Some(sid) = latest {
                    self.undo_sweep(&sid, tx);
                }
            }
            PaletteAction::SetStrategy(st) => self.strategy = st,
            PaletteAction::SetHero(h) => {
                self.hero_variant = h;
                self.route = NavRoute::Overview;
            }
            PaletteAction::SetDataset(ds) => self.switch_dataset(ds),
            PaletteAction::ToggleDark => {
                let next = !self.dark_override.unwrap_or(true);
                self.dark_override = Some(next);
            }
            PaletteAction::ToggleDevHarness => {
                self.dev_harness_open = !self.dev_harness_open;
            }
        }
    }
}

/// Frame-level `poll` hook: drains background live-scan messages, steps Hold-to-Sweep progress,
/// and advances `ToastStack` timers deterministically on both real and headless virtual clocks!
pub fn poll(s: &mut ClearanceApp, now: f64, tx: &mut Tx) -> bool {
    let mut changed = false;

    // 1. Drain non-blocking live scanner channel (up to 64 items per frame to stay well inside 120fps budget)
    let mut finished_scan = false;
    if let Some(handle) = &s.scan_handle {
        let mut new_items = false;
        for _ in 0..64 {
            match handle.rx.try_recv() {
                Ok(ScanMessage::Disk(disk)) => {
                    s.disk = disk;
                    changed = true;
                }
                Ok(ScanMessage::Progress {
                    phase,
                    scanned_dirs,
                }) => {
                    s.scan_phase = phase;
                    s.scanned_dirs = scanned_dirs;
                    changed = true;
                }
                Ok(ScanMessage::Item(item)) => {
                    s.items.push(item);
                    new_items = true;
                    changed = true;
                }
                Ok(ScanMessage::Finished { elapsed_ms }) => {
                    s.scanning = false;
                    s.last_scan_ms = Some(elapsed_ms);
                    s.scan_phase = "Scan complete".into();
                    finished_scan = true;
                    changed = true;
                    break;
                }
                Err(_) => break,
            }
        }
        if new_items || finished_scan {
            s.recompute_visible();
            if s.inspected_id.is_none()
                && let Some(&first) = s.visible_indices.first()
            {
                let id = s.items[first].id.clone();
                s.inspected_id = Some(id.clone());
                s.table.select(id);
            }
        }
    }
    if finished_scan {
        s.scan_handle = None;
    }

    // 1b. Drain the whole-disk map scanner.
    if s.map.poll() {
        changed = true;
        if s.map_ui.selected.is_none() && s.map.root.is_some() {
            s.map_ui.selected = s.map.dir_at(&s.map_ui.nav).map(Sel::from_node);
        }
    }

    // 2. Step Asymmetric Hold-to-Sweep progress (`1.05s` linear ramp while held)
    if s.hold_sweep.holding && !s.hold_sweep.fired_this_press {
        let elapsed = (now - s.hold_sweep.started_at).max(0.0);
        let prog = (elapsed / HoldSweepState::HOLD_DURATION_SECS).clamp(0.0, 1.0) as f32;
        s.hold_sweep.progress = prog;
        changed = true;
        if prog >= 1.0 {
            s.trigger_sweep(tx);
        } else {
            tx.wake_after(Duration::from_millis(16));
        }
    }

    // 3. Step ToastStack auto-dismiss timers
    for eff in s.toasts.step(ToastEvent::Tick { now }) {
        match eff {
            ToastEffect::Closed(_) | ToastEffect::Pulse(_) => changed = true,
            ToastEffect::WakeAt(due) => tx.wake_at(due),
        }
    }

    changed
}

fn handle_palette_key(s: &mut ClearanceApp, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.is_command('k') {
        s.palette_open = false;
        tx.instant();
        return true;
    }
    match &ev.key {
        Key::Escape => {
            s.palette_open = false;
            tx.instant();
            true
        }
        Key::Down => {
            let len = s.filtered_palette_commands().len();
            if len > 0 {
                s.palette_cursor = (s.palette_cursor + 1) % len;
            }
            tx.instant();
            true
        }
        Key::Up => {
            let len = s.filtered_palette_commands().len();
            if len > 0 {
                s.palette_cursor = if s.palette_cursor == 0 {
                    len - 1
                } else {
                    s.palette_cursor - 1
                };
            }
            tx.instant();
            true
        }
        Key::Enter => {
            let cmds = s.filtered_palette_commands();
            if let Some(picked) = cmds.get(s.palette_cursor).cloned() {
                s.execute_palette_action(picked.action, tx);
            }
            true
        }
        _ => false,
    }
}

/// Global keyboard shortcuts (`remove-animations`: keyboard actions execute at `0ms` via `tx.instant()`).
pub fn on_key(s: &mut ClearanceApp, ev: &KeyEvent, tx: &mut Tx) -> bool {
    // ⌘K toggles Command Palette at 0ms!
    if ev.is_command('k') {
        s.palette_open = !s.palette_open;
        if s.palette_open {
            s.palette_query = TextEdit::new("");
            s.palette_query.focused = true;
            s.palette_cursor = 0;
        }
        tx.instant();
        return true;
    }

    if s.palette_open && handle_palette_key(s, ev, tx) {
        return true;
    }

    if shell::is_sidebar_toggle(ev) {
        s.sidebar.toggle();
        return true;
    }
    if ev.is_command('i') && ev.mods.alt {
        s.inspector.toggle();
        return true;
    }
    if ev.is_command('d') {
        s.dev_harness_open = !s.dev_harness_open;
        tx.instant();
        return true;
    }
    if ev.is_command('r') {
        s.begin_live_scan();
        tx.instant();
        return true;
    }
    if ev.is_command('l') && ev.mods.shift {
        let next = !s.dark_override.unwrap_or(true);
        s.dark_override = Some(next);
        tx.instant();
        return true;
    }
    if ev.is_command('1') {
        s.route = NavRoute::Overview;
        s.recompute_visible();
        tx.instant();
        return true;
    }
    if ev.is_command('2') {
        s.route = NavRoute::DiskMap;
        tx.instant();
        return true;
    }
    if ev.is_command('3') {
        s.route = NavRoute::AllFindings;
        s.recompute_visible();
        tx.instant();
        return true;
    }
    if ev.is_command('4') {
        s.route = NavRoute::QuarantineVault;
        tx.instant();
        return true;
    }
    if ev.is_command('5') {
        s.route = NavRoute::SafetyConstitution;
        tx.instant();
        return true;
    }

    match &ev.key {
        Key::Escape | Key::Backspace if s.route == NavRoute::DiskMap && tx.focused().is_none() => {
            if s.map_go_up() {
                tx.instant();
                return true;
            }
        }
        Key::Escape if matches!(s.route, NavRoute::CategoryFocus(_)) => {
            s.route = NavRoute::Overview;
            s.recompute_visible();
            tx.animated();
            tx.motion(UI_SPRING);
            return true;
        }
        Key::Text(ch) if ch == " " && tx.focused() == Some("findings-table") => {
            if let Some(cur_id) = s.table.cursor().map(str::to_owned) {
                s.toggle_item_by_id(&cur_id);
                tx.instant();
                return true;
            }
        }
        _ => {}
    }

    false
}

// ===========================================================================
// Main Rune View
// ===========================================================================

pub fn view(s: &ClearanceApp, cx: &mut Cx<ClearanceApp>) -> El {
    let dark = s.dark_override.unwrap_or_else(|| cx.system_dark());
    if let Some(rm) = s.reduced_motion_override {
        cx.reduced_motion(rm);
    }
    let reduced_motion = cx.is_reduced_motion();
    let k = clearance_kit(dark);

    let sidebar_el = build_sidebar(s, &k);
    let main_el = build_main_stage(s, reduced_motion, &k);
    let inspector_el = if s.route == NavRoute::DiskMap {
        build_map_inspector(s, &k)
    } else {
        build_inspector(s, &k)
    };
    let toolbar = build_toolbar(s, dark, &k);

    let shell = SplitView::new(main_el)
        .sidebar(&s.sidebar, |s: &mut ClearanceApp| &mut s.sidebar, sidebar_el)
        .inspector(
            &s.inspector,
            |s: &mut ClearanceApp| &mut s.inspector,
            inspector_el,
        )
        .toolbar(toolbar)
        .titlebar_inset(cx.titlebar_inset())
        .frame_inset(8.0)
        .toolbar_in_panel()
        .sidebar_padding(16.0, 10.0)
        .sidebar_toggle(
            SidebarToggle::new()
                .size(ControlSize::Large)
                .past_lights(14.0)
                .edge(16.0)
                .gap(4.0)
                .in_header(),
        )
        .pane_motion(
            PaneMotion::new()
                .spring(Spring::new(0.24, 0.0))
                .scale_with_width(480.0, 0.14, 0.26)
                .fade_tail(0.12),
        )
        .sidebar_peek(
            SidebarPeek::new()
                .edge(14.0, Duration::from_millis(40))
                .toggle_delay(Duration::from_millis(80))
                .below(52.0)
                .hold(14.0, Duration::from_millis(100))
                .spring(Spring::new(0.18, 0.0)),
        )
        .reduced_motion(reduced_motion)
        .build(&k);

    let mut overlays = Vec::new();
    if s.dev_harness_open {
        overlays.push(build_dev_harness_overlay(s, &k));
    }
    if let Some(toast_overlay) = build_toast_overlay(s, &k) {
        overlays.push(toast_overlay);
    }
    if s.palette_open {
        overlays.push(build_command_palette(s, &k));
    }

    rune_kit::root(shell, overlays)
}

fn build_toolbar(s: &ClearanceApp, dark: bool, k: &Kit) -> Toolbar {
    let title = match s.route {
        NavRoute::Overview => "Storage Overview",
        NavRoute::DiskMap => "Disk Map",
        NavRoute::AllFindings => "All Storage Findings",
        NavRoute::CategoryFocus(cat) => cat.title(),
        NavRoute::QuarantineVault => "Quarantine Vault",
        NavRoute::SafetyConstitution => "Safety Constitution",
    };

    let proto_idx = HeroVariant::ALL
        .iter()
        .position(|&v| v == s.hero_variant);
    let proto_labels: [&str; 3] = ["Bento", "Ledger", "Radar"];

    let mut trailing = row().center_y().gap(6.0);

    if s.route == NavRoute::Overview {
        trailing = trailing.child(
            SegmentedControl::new(&proto_labels, proto_idx)
                .label("Analyzer Layout")
                .fit()
                .on_pick(|s: &mut ClearanceApp, idx, tx| {
                    s.hero_variant = HeroVariant::ALL[idx];
                    tx.instant();
                })
                .build(k)
                .key("proto-switcher"),
        );
    }

    // Rescan Live Mac button
    let scan_label = if s.scanning || s.map.scanning {
        "Scanning…"
    } else {
        "Scan Mac"
    };
    let scan_label = scan_label.replace("Mac", platform::DEVICE);
    trailing = trailing.child(
        Button::new(scan_label)
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Regular)
            .icon(&ICON_REFRESH)
            .on_press_tx(|s: &mut ClearanceApp, tx| {
                s.begin_live_scan();
                tx.instant();
            })
            .build(k)
            .key("toolbar-scan-btn"),
    );

    // ⌘K Command Palette pill
    trailing = trailing.child(
        Button::new(platform::kbd("⌘K"))
            .variant(ButtonVariant::Outline)
            .control_size(ControlSize::Regular)
            .icon(&glyphs::SEARCH)
            .on_press_tx(|s: &mut ClearanceApp, tx| {
                s.palette_open = true;
                s.palette_query = TextEdit::new("");
                s.palette_query.focused = true;
                s.palette_cursor = 0;
                tx.instant();
            })
            .build(k)
            .key("toolbar-cmdk-btn"),
    );

    // Appearance toggle (Dark / Light)
    trailing = trailing.child(
        IconButton::new(if dark { &ICON_SUN } else { &ICON_MOON })
            .variant(IconButtonVariant::GhostMuted)
            .help(platform::kbd(if dark {
                "Switch to Light Appearance (⇧⌘L)"
            } else {
                "Switch to Dark Appearance (⇧⌘L)"
            }))
            .on_press_tx(move |s: &mut ClearanceApp, tx| {
                let cur = s.dark_override.unwrap_or(dark);
                s.dark_override = Some(!cur);
                tx.instant();
            })
            .build(k)
            .key("toolbar-theme-btn"),
    );

    // Inspector toggle
    trailing = trailing.child(
        IconButton::new(&shell::INSPECTOR)
            .variant(IconButtonVariant::GhostMuted)
            .expanded(s.inspector.is_open())
            .help(platform::kbd("Toggle File Inspector (⌥⌘I)"))
            .on_press(|s: &mut ClearanceApp| s.inspector.toggle())
            .build(k)
            .key("toolbar-inspector-btn"),
    );

    Toolbar::new()
        .leading(
            text(title)
                .font(tabular_ui_medium(k))
                .color(k.c.text_base)
                .truncate(),
        )
        .trailing(trailing)
}

fn build_sidebar(s: &ClearanceApp, k: &Kit) -> El {
    let stats = summarize_categories(&s.items, s.horizon.days);
    let total_all: u64 = stats.iter().map(|st| st.total_bytes).sum();
    let vault_n = s.vault_count();

    let mut sb = Sidebar::new()
        .label("Cleanse Navigation")
        .inset(0.0)
        .header_gap(3.0)
        .header_padding(0.0, 10.0)
        .row_gap(2.0)
        .header(
            SidebarRow::new("Smart Overview")
                .icon(&ICON_GRID)
                .detail(format_bytes(total_all))
                .height(32.0)
                .selected(s.route == NavRoute::Overview)
                .on_select(|s: &mut ClearanceApp, tx| {
                    s.route = NavRoute::Overview;
                    s.recompute_visible();
                    tx.instant();
                })
                .build(k)
                .key("nav-overview"),
        )
        .header(
            SidebarRow::new("Disk Map")
                .icon(&ICON_MAP)
                .detail(
                    s.map
                        .root
                        .as_ref()
                        .map_or("—".to_owned(), |r| format_bytes(r.bytes)),
                )
                .height(32.0)
                .selected(s.route == NavRoute::DiskMap)
                .on_select(|s: &mut ClearanceApp, tx| {
                    s.route = NavRoute::DiskMap;
                    tx.instant();
                })
                .build(k)
                .key("nav-diskmap"),
        )
        .header(
            SidebarRow::new("All Findings")
                .icon(&ICON_LIST)
                .detail(format_count(s.items.len()))
                .height(32.0)
                .selected(s.route == NavRoute::AllFindings)
                .on_select(|s: &mut ClearanceApp, tx| {
                    s.route = NavRoute::AllFindings;
                    s.recompute_visible();
                    tx.instant();
                })
                .build(k)
                .key("nav-all-findings"),
        );

    sb = sb.child(
        SidebarSection::new("STORAGE CATEGORIES")
            .build(k)
            .key("sec-cats"),
    );

    for cat in Category::ALL {
        let bytes = stats
            .iter()
            .find(|st| st.category == cat)
            .map_or(0, |st| st.total_bytes);
        let dot_color = category_accent(cat, k.dark);
        let dot = el().size(8.0, 8.0).radius(4.0).fill(dot_color);
        let detail = if bytes == 0 {
            "—".into()
        } else {
            format_bytes(bytes)
        };
        sb = sb.child(
            SidebarRow::new(cat.short_title())
                .avatar(dot)
                .detail(detail)
                .height(31.0)
                .selected(s.route == NavRoute::CategoryFocus(cat))
                .on_select(move |s: &mut ClearanceApp, tx| {
                    s.route = NavRoute::CategoryFocus(cat);
                    s.recompute_visible();
                    tx.instant();
                })
                .build(k)
                .key(format!("nav-cat-{}", cat.id())),
        );
    }

    // Footer: Quarantine Vault + Safety Constitution + Dev Harness toggle
    let vault_detail = if vault_n == 0 {
        "Empty".into()
    } else {
        pluralize(vault_n, "sweep", "sweeps")
    };

    sb.footer(
        col()
            .w_full()
            .gap(2.0)
            .child(
                SidebarRow::new("Quarantine Vault")
                    .icon(&ICON_VAULT)
                    .detail(vault_detail)
                    .height(31.0)
                    .selected(s.route == NavRoute::QuarantineVault)
                    .on_select(|s: &mut ClearanceApp, tx| {
                        s.route = NavRoute::QuarantineVault;
                        tx.instant();
                    })
                    .build(k)
                    .key("nav-vault"),
            )
            .child(
                SidebarRow::new("Safety Constitution")
                    .icon(&ICON_SHIELD)
                    .detail("5 Layers")
                    .height(31.0)
                    .selected(s.route == NavRoute::SafetyConstitution)
                    .on_select(|s: &mut ClearanceApp, tx| {
                        s.route = NavRoute::SafetyConstitution;
                        tx.instant();
                    })
                    .build(k)
                    .key("nav-safety"),
            )
            .child(
                SidebarRow::new(format!("Dataset · {}", s.dataset.label()))
                    .icon(&glyphs::SPARKLE)
                    .detail(platform::kbd("⌘D"))
                    .height(29.0)
                    .selected(s.dev_harness_open)
                    .on_select(|s: &mut ClearanceApp, tx| {
                        s.dev_harness_open = !s.dev_harness_open;
                        tx.instant();
                    })
                    .build(k)
                    .key("nav-harness"),
            ),
    )
    .build(k)
}

fn build_main_stage(s: &ClearanceApp, reduced_motion: bool, k: &Kit) -> El {
    match s.route {
        NavRoute::DiskMap => build_map_view(s, k),
        NavRoute::QuarantineVault => build_vault_view(s, k),
        NavRoute::SafetyConstitution => build_constitution_view(k),
        NavRoute::Overview | NavRoute::AllFindings | NavRoute::CategoryFocus(_) => {
            build_analyzer_and_table_stage(s, reduced_motion, k)
        }
    }
}

fn build_analyzer_and_table_stage(s: &ClearanceApp, reduced_motion: bool, k: &Kit) -> El {
    let c = &k.c;
    let (sel_count, sel_bytes) = s.selected_reclaimable_bytes();
    let review_extra = s.reviewable_unselected_bytes();
    let stats = summarize_categories(&s.items, s.horizon.days);

    let mut stage = col()
        .key("main-analyzer-stage")
        .w_full()
        .h_full()
        .min_h(0.0)
        .padding(18.0)
        .gap(14.0);

    match s.route {
        NavRoute::Overview => {
            // Top KPI + Horizon Bar card
            let scan_badge = if s.scanning {
                format!("Scanning · {} dirs", format_count(s.scanned_dirs))
            } else if let Some(ms) = s.last_scan_ms {
                format!(
                    "{} · {} · {} ms",
                    s.disk.volume_name,
                    s.dataset.label(),
                    ms
                )
            } else {
                s.disk.volume_name.clone()
            };

            let summary_header = col()
                .key("overview-summary-card")
                .w_full()
                .padding(16.0)
                .gap(12.0)
                .radius(12.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(
                    row()
                        .w_full()
                        .center_y()
                        .gap(16.0)
                        .child(
                            col()
                                .grow(1.0)
                                .shrink()
                                .gap(2.0)
                                .child(
                                    row()
                                        .center_y()
                                        .gap(8.0)
                                        .child(
                                            text("SAFE RECLAIMABLE SPACE")
                                                .font(tabular_small_medium(k))
                                                .color(c.text_faint),
                                        )
                                        .child(
                                            text(scan_badge)
                                                .font(tabular_small(k))
                                                .color(c.text_muted)
                                                .truncate(),
                                        ),
                                )
                                .child(
                                    row()
                                        .center_y()
                                        .gap(10.0)
                                        .child(
                                            RollingText::new(
                                                "hero-reclaimable-bytes",
                                                format_bytes(sel_bytes),
                                            )
                                            .value(sel_bytes as f64)
                                            .font(hero_metric_font(k))
                                            .color(c.text_base)
                                            .spring(Spring::new(0.28, 0.0))
                                            .build(k),
                                        )
                                        .child(
                                            text(format!(
                                                "across {} staged",
                                                pluralize(sel_count, "item", "items")
                                            ))
                                            .font(tabular_ui(k))
                                            .color(c.text_muted),
                                        ),
                                ),
                        )
                        .child(dormancy_slider(
                            s.horizon,
                            k,
                            |s: &mut ClearanceApp| &mut s.horizon,
                            |s: &mut ClearanceApp| s.recompute_visible(),
                        )),
                )
                .child(storage_horizon_bar(&s.disk, sel_bytes, review_extra, k));

            let hero_viz = match s.hero_variant {
                HeroVariant::SpatialBento => {
                    spatial_bento_map(&stats, k, |s: &mut ClearanceApp, cat, tx| {
                        s.route = NavRoute::CategoryFocus(cat);
                        s.recompute_visible();
                        tx.animated();
                        tx.motion(UI_SPRING);
                    })
                }
                HeroVariant::HorizonLedger => {
                    horizon_ledger_view(&stats, k, |s: &mut ClearanceApp, cat, tx| {
                        s.route = NavRoute::CategoryFocus(cat);
                        s.recompute_visible();
                        tx.animated();
                        tx.motion(UI_SPRING);
                    })
                }
                HeroVariant::RunwayRadar => {
                    runway_radar_view(&s.disk, &s.items, sel_bytes, k)
                }
            };

            stage = stage.child(summary_header).child(hero_viz);
        }
        NavRoute::CategoryFocus(cat) => {
            let cat_stat = stats
                .iter()
                .find(|st| st.category == cat)
                .copied()
                .unwrap_or(crate::ui::charts::CategoryStat {
                    category: cat,
                    total_bytes: 0,
                    selected_bytes: 0,
                    item_count: 0,
                });
            let accent = category_accent(cat, k.dark);
            let banner = row()
                .key(format!("cat-banner-{}", cat.id()))
                .matched(format!("cat-card-{}", cat.id()))
                .matched_crop()
                .w_full()
                .padding(16.0)
                .gap(14.0)
                .center_y()
                .radius(12.0)
                .fill(category_surface(cat, k.dark, true))
                .border(1.0, category_border(cat, k.dark, true))
                .child(
                    Button::new("Overview")
                        .variant(ButtonVariant::Outline)
                        .control_size(ControlSize::Regular)
                        .on_press_tx(|s: &mut ClearanceApp, tx| {
                            s.route = NavRoute::Overview;
                            s.recompute_visible();
                            tx.animated();
                            tx.motion(UI_SPRING);
                        })
                        .build(k)
                        .key("cat-back-btn"),
                )
                .child(icon(category_icon(cat), 18.0).color(accent))
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .gap(2.0)
                        .child(
                            text(cat.title())
                                .font(tabular_ui_medium(k))
                                .color(c.text_base)
                                .truncate(),
                        )
                        .child(
                            text(cat.tagline())
                                .font(tabular_small(k))
                                .color(c.text_muted)
                                .truncate(),
                        ),
                )
                .child(
                    col()
                        .items_end()
                        .gap(2.0)
                        .child(
                            text(format_bytes(cat_stat.total_bytes))
                                .font(tabular_ui_medium(k))
                                .color(c.text_base),
                        )
                        .child(
                            text(pluralize(cat_stat.item_count, "item", "items"))
                                .font(tabular_small(k))
                                .color(c.text_muted),
                        ),
                );
            stage = stage.child(banner);
        }
        _ => {}
    }

    // Filter bar + Virtualized Findings Table
    let mut search_input = TextInput::new(&s.search)
        .placeholder("Filter paths, projects, bundle IDs…")
        .leading_icon(&glyphs::SEARCH)
        .width(Some(284.0))
        .on_edit(|s: &mut ClearanceApp| &mut s.search)
        .on_change(|s: &mut ClearanceApp, tx| {
            s.recompute_visible();
            tx.instant();
        })
        .on_action(|s: &mut ClearanceApp, tx| {
            s.search = TextEdit::new("");
            s.recompute_visible();
            tx.instant();
        });
    if !s.search.text.is_empty() {
        search_input = search_input.clear_button();
    }

    let filter_bar = row()
        .key("findings-filter-bar")
        .w_full()
        .center_y()
        .gap(10.0)
        .child(search_input.build(k).key("findings-search-input"))
        .child(
            Button::new("Safe Only")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Regular)
                .on_press_tx(|s: &mut ClearanceApp, tx| {
                    s.select_safe_regenerable_only();
                    tx.instant();
                })
                .build(k)
                .key("btn-safe-only"),
        )
        .child(
            Button::new("Clear")
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Regular)
                .on_press_tx(|s: &mut ClearanceApp, tx| {
                    s.clear_all_selections();
                    tx.instant();
                })
                .build(k)
                .key("btn-clear-sel"),
        )
        .child(spacer())
        .child(
            text(format!(
                "Showing {}",
                pluralize(s.visible_indices.len(), "finding", "findings")
            ))
            .font(tabular_small(k))
            .color(c.text_muted),
        );

    let table_container = build_findings_table(s, k);
    let dock = build_sweep_dock(s, sel_count, sel_bytes, reduced_motion, k);

    stage
        .child(filter_bar)
        .child(table_container)
        .child(dock)
}

fn build_findings_table(s: &ClearanceApp, k: &Kit) -> El {
    if s.visible_indices.is_empty() {
        let msg = if s.scanning {
            s.scan_phase.as_str()
        } else {
            "No storage items match your current search or dormancy filter."
        };
        return col()
            .key("findings-empty-box")
            .grow(1.0)
            .min_h(0.0)
            .w_full()
            .center()
            .gap(6.0)
            .radius(10.0)
            .fill(k.c.layer_01)
            .border(1.0, k.c.border_weak)
            .child(
                text(if s.scanning {
                    format!("Scanning {} Storage…", platform::OS_NAME)
                } else {
                    "Nothing to Show".to_owned()
                })
                .font(tabular_ui_medium(k))
                .color(k.c.text_base),
            )
            .child(text(msg).font(tabular_small(k)).color(k.c.text_muted));
    }

    let kit_cell = k.clone();
    let mut style = TableStyle::from_kit(k);
    style.row_height = 34.0;
    style.header_height = 30.0;

    let tbl = Table::new(
        |s: &ClearanceApp| &s.table,
        |s: &mut ClearanceApp| &mut s.table,
        |s: &ClearanceApp| s.visible_indices.len(),
        |s: &ClearanceApp, row_i| {
            let idx = s.visible_indices[row_i];
            Cow::Borrowed(s.items[idx].id.as_str())
        },
    )
    .name("Storage Findings")
    .style(style)
    .column(
        TableColumn::new("include", "")
            .width(46.0)
            .resizable(false),
    )
    .column(
        TableColumn::new("name", "Artifact / Location")
            .flex(2.4)
            .min(180.0)
            .sortable(),
    )
    .column(
        TableColumn::new("category", "Category")
            .width(128.0)
            .sortable(),
    )
    .column(
        TableColumn::new("safety", "Safety Proof")
            .width(142.0)
            .sortable(),
    )
    .column(
        TableColumn::new("age", "Dormancy")
            .width(84.0)
            .align(Align::End)
            .sortable(),
    )
    .column(
        TableColumn::new("size", "Size on Disk")
            .width(116.0)
            .align(Align::End)
            .sortable(),
    )
    .cell(move |s: &ClearanceApp, row_i, col_id| {
        let Some(&item_idx) = s.visible_indices.get(row_i) else {
            return Cell::text("");
        };
        let it = &s.items[item_idx];
        match col_id {
            "include" => {
                if !it.is_selectable() {
                    let lock_reason = if let Some(p) = &it.in_use_by {
                        format!("Locked: {p} is currently running")
                    } else {
                        "Protected · Inspect Only (never batch-cleaned)".into()
                    };
                    Cell::el(
                        row()
                            .w_full()
                            .center()
                            .help(lock_reason)
                            .child(icon(&glyphs::LOCK, 13.0).color(kit_cell.c.icon_faint)),
                    )
                } else {
                    let item_id = it.id.clone();
                    Cell::el(
                        row().w_full().center().child(
                            Checkbox::new(it.selected)
                                .label(format!("Include {}", it.name))
                                .hide_label()
                                .on_toggle_tx(move |s: &mut ClearanceApp, tx| {
                                    s.toggle_item_by_id(&item_id);
                                    tx.instant();
                                })
                                .build(&kit_cell),
                        ),
                    )
                }
            }
            "name" => {
                let path_str = it.path.to_string_lossy().into_owned();
                Cell::el(
                    col()
                        .w_full()
                        .shrink()
                        .gap(1.0)
                        .help(format!("{}\n{}", it.subtitle, path_str))
                        .child(
                            text(it.name.clone())
                                .font(tabular_ui_medium(&kit_cell))
                                .color(kit_cell.c.text_base)
                                .truncate(),
                        )
                        .child(
                            text(path_str)
                                .font(tabular_small(&kit_cell))
                                .color(kit_cell.c.text_faint)
                                .truncate_middle(),
                        ),
                )
            }
            "category" => {
                let acc = category_accent(it.category, kit_cell.dark);
                Cell::el(
                    row()
                        .center_y()
                        .gap(6.0)
                        .child(el().size(7.0, 7.0).radius(3.5).fill(acc))
                        .child(
                            text(it.category.short_title())
                                .font(tabular_small_medium(&kit_cell))
                                .color(kit_cell.c.text_muted)
                                .truncate(),
                        ),
                )
            }
            "safety" => Cell::el(safety_pill(
                it.safety,
                it.in_use_by.as_deref(),
                true,
                &kit_cell,
            )),
            "age" => Cell::text(format_age(it.age_days)).mono().muted(),
            "size" => {
                let is_sparse = it.logical_bytes > it.bytes.saturating_mul(2);
                let label = if is_sparse {
                    format!("{}*", format_bytes(it.bytes))
                } else {
                    format_bytes(it.bytes)
                };
                Cell::el(
                    row()
                        .w_full()
                        .justify_end()
                        .pr(10.0)
                        .child(
                            text(label)
                                .font(kit_cell.ty.mono.features("'tnum' 1"))
                                .color(kit_cell.c.text_base),
                        ),
                )
            }
            _ => Cell::text(""),
        }
    })
    .on_sort(|s: &mut ClearanceApp, col_id, dir, tx| {
        s.sort_col = col_id.to_owned();
        s.sort_dir = dir;
        s.recompute_visible();
        tx.instant();
    })
    .on_select(|s: &mut ClearanceApp, id, tx| {
        s.inspected_id = Some(id.to_owned());
        tx.instant();
    })
    .on_open(|s: &mut ClearanceApp, id, tx| {
        s.inspected_id = Some(id.to_owned());
        s.toggle_item_by_id(id);
        tx.instant();
    })
    .build(s, k);

    col()
        .key("findings-table-shell")
        .grow(1.0)
        .min_h(160.0)
        .w_full()
        .radius(10.0)
        .clip_rounded(10.0)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_weak)
        .child(tbl.grow(1.0).min_h(0.0))
}

/// Translucent floating Sweep Dock (`apple-design` material + `RollingText` + `CleanStrategy` selector + Asymmetric Hold-to-Sweep).
fn build_sweep_dock(
    s: &ClearanceApp,
    sel_count: usize,
    sel_bytes: u64,
    reduced_motion: bool,
    k: &Kit,
) -> El {
    let tint = if k.dark {
        oklcha(0.20, 0.015, 255.0, 0.84)
    } else {
        oklcha(0.97, 0.006, 255.0, 0.88)
    };

    let strat_idx = CleanStrategy::ALL
        .iter()
        .position(|&st| st == s.strategy);
    let strat_labels = ["Vault", "Trash", "Dry Run"];

    row()
        .key("sweep-action-dock")
        .w_full()
        .padding(12.0)
        .px(16.0)
        .gap(12.0)
        .center_y()
        .radius(12.0)
        .backdrop_blur(10.0)
        .fill(tint)
        .border(1.0, k.c.border_base)
        .child(
            col()
                .grow(1.0)
                .shrink()
                .gap(2.0)
                .child(
                    row()
                        .center_y()
                        .gap(6.0)
                        .child(
                            RollingText::new("dock-reclaim-bytes", format_bytes(sel_bytes))
                                .value(sel_bytes as f64)
                                .font(tabular_ui_medium(k))
                                .color(k.c.text_base)
                                .spring(Spring::new(0.26, 0.0))
                                .build(k),
                        )
                        .child(
                            text(format!(
                                "· {}",
                                pluralize(sel_count, "item", "items")
                            ))
                            .font(tabular_small(k))
                            .color(k.c.text_muted)
                            .truncate(),
                        ),
                )
                .child(
                    text(match s.strategy {
                        CleanStrategy::QuarantineVault => {
                            "Undoable via ~/.clearance-vault"
                        }
                        CleanStrategy::MacTrash => {
                            if platform::IS_WINDOWS { "Moves to the Recycle Bin" } else { "Moves to macOS ~/.Trash" }
                        }
                        CleanStrategy::DryRun => {
                            "Simulates safety · zero files moved"
                        }
                    })
                    .font(tabular_small(k))
                    .color(k.c.text_faint)
                    .truncate(),
                ),
        )
        .child(
            SegmentedControl::new(&strat_labels, strat_idx)
                .label("Reclamation Strategy")
                .fit()
                .on_pick(|s: &mut ClearanceApp, idx, tx| {
                    s.strategy = CleanStrategy::ALL[idx];
                    tx.instant();
                })
                .build(k)
                .key("dock-strategy-seg"),
        )
        .child(hold_to_sweep_button(
            s.hold_sweep,
            s.strategy,
            sel_count > 0,
            reduced_motion,
            k,
            |s: &mut ClearanceApp| &mut s.hold_sweep,
            |s: &mut ClearanceApp, tx| s.trigger_sweep(tx),
        ))
}

// ===========================================================================
// Right Inspector Drawer
// ===========================================================================

fn build_inspector(s: &ClearanceApp, k: &Kit) -> El {
    let c = &k.c;
    let inspected = s
        .inspected_id
        .as_deref()
        .and_then(|id| s.items.iter().find(|x| x.id == id));

    let Some(it) = inspected else {
        return col()
            .key("inspector-empty")
            .w_full()
            .h_full()
            .padding(20.0)
            .center()
            .gap(6.0)
            .child(
                text("No Item Selected")
                    .font(tabular_ui_medium(k))
                    .color(c.text_base),
            )
            .child(
                text("Select any storage finding to inspect its on-disk size, safety proof, and rebuild command.")
                    .font(tabular_small(k))
                    .color(c.text_muted)
                    .wrap(),
            );
    };

    let path_str = it.path.to_string_lossy().into_owned();
    let is_sparse = it.logical_bytes > it.bytes.saturating_add(1_000_000);
    let item_id = it.id.clone();
    let reveal_path = it.path.clone();
    let copy_path = path_str.clone();

    let kv = |label_s: &str, val_s: String| -> El {
        row()
            .w_full()
            .justify_between()
            .center_y()
            .gap(8.0)
            .child(
                text(label_s.to_owned())
                    .font(tabular_small(k))
                    .color(c.text_faint),
            )
            .child(
                text(val_s)
                    .font(tabular_small_medium(k))
                    .color(c.text_base)
                    .truncate(),
            )
    };

    let mut body = col()
        .key("inspector-pane")
        .w_full()
        .h_full()
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .padding(16.0)
        .gap(14.0)
        .child(
            col()
                .w_full()
                .gap(6.0)
                .child(safety_pill(
                    it.safety,
                    it.in_use_by.as_deref(),
                    false,
                    k,
                ))
                .child(
                    text(it.name.clone())
                        .font(tabular_ui_medium(k))
                        .color(c.text_base)
                        .wrap_anywhere(),
                )
                .child(
                    text(it.subtitle.clone())
                        .font(tabular_small(k))
                        .color(c.text_muted)
                        .wrap(),
                ),
        )
        // APFS Metrics Box
        .child(
            col()
                .w_full()
                .padding(12.0)
                .gap(7.0)
                .radius(9.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(kv("Allocated on Disk", format_bytes(it.bytes)))
                .child(kv("Apparent Logical", format_bytes(it.logical_bytes)))
                .child(kv(
                    "Files Inside",
                    pluralize(it.file_count as usize, "file", "files"),
                ))
                .child(kv("Last Activity", format_age(it.age_days)))
                .child(kv("Category", it.category.title().into())),
        );

    if is_sparse {
        body = body.child(
            col()
                .w_full()
                .padding(10.0)
                .gap(3.0)
                .radius(8.0)
                .fill(oklcha(0.62, 0.12, 205.0, if k.dark { 0.18 } else { 0.14 }))
                .border(1.0, oklcha(0.62, 0.12, 205.0, 0.35))
                .child(
                    text("Sparse File Detected")
                        .font(tabular_small_medium(k))
                        .color(c.text_base),
                )
                .child(
                    text(format!(
                        "{} reports {} logical size, but only {} is allocated on disk. Cleanse always measures true physical blocks.",
                        platform::FILE_MANAGER,
                        format_bytes(it.logical_bytes),
                        format_bytes(it.bytes)
                    ))
                    .font(tabular_small(k))
                    .color(c.text_muted)
                    .wrap(),
                ),
        );
    }

    // Path Box with Copy + Reveal in Finder
    body = body.child(
        col()
            .w_full()
            .padding(12.0)
            .gap(8.0)
            .radius(9.0)
            .fill(c.layer_01)
            .border(1.0, c.border_weak)
            .child(
                text("FILESYSTEM PATH")
                    .font(tabular_small_medium(k))
                    .color(c.text_faint),
            )
            .child(
                text(path_str)
                    .font(k.ty.mono.features("'tnum' 1"))
                    .color(c.text_base)
                    .wrap_anywhere(),
            )
            .child(
                row()
                    .w_full()
                    .gap(8.0)
                    .child(
                        Button::new("Copy Path")
                            .variant(ButtonVariant::Outline)
                            .control_size(ControlSize::Regular)
                            .icon(&glyphs::COPY)
                            .on_press_tx(move |s: &mut ClearanceApp, tx| {
                                tx.copy(copy_path.clone());
                                s.push_toast(
                                    "copied-path",
                                    ToastPayload {
                                        title: "Copied Path".into(),
                                        description: copy_path.clone(),
                                        undo_sweep_id: None,
                                    },
                                    tx.clock(),
                                    tx,
                                );
                            })
                            .build(k)
                            .key("insp-copy-path"),
                    )
                    .child(
                        Button::new(format!("Reveal in {}", platform::FILE_MANAGER))
                            .variant(ButtonVariant::Ghost)
                            .control_size(ControlSize::Regular)
                            .on_press(move |_: &mut ClearanceApp| {
                                reveal_in_finder(&reveal_path);
                            })
                            .build(k)
                            .key("insp-reveal"),
                    ),
            ),
    );

    // Rebuild Hint Box
    if let Some(hint) = &it.rebuild_hint {
        let hint_copy = hint.clone();
        let is_cli = hint.starts_with("cargo ")
            || hint.starts_with("pnpm ")
            || hint.starts_with("npm ")
            || hint.starts_with("yarn ")
            || hint.starts_with("bun ")
            || hint.starts_with("uv ")
            || hint.starts_with("brew ")
            || hint.starts_with("./gradlew");
        let mut rebuild_card = col()
            .w_full()
            .padding(12.0)
            .gap(6.0)
            .radius(9.0)
            .fill(c.layer_01)
            .border(1.0, c.border_weak)
            .child(
                text("REGENERATION / SAFETY PROOF")
                    .font(tabular_small_medium(k))
                    .color(c.text_faint),
            )
            .child(
                text(hint.clone())
                    .font(if is_cli {
                        k.ty.mono.features("'tnum' 1")
                    } else {
                        tabular_small(k)
                    })
                    .color(c.text_base)
                    .wrap(),
            );
        if is_cli {
            rebuild_card = rebuild_card.child(
                Button::new("Copy Rebuild Command")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Regular)
                    .icon(&glyphs::COPY)
                    .on_press_tx(move |s: &mut ClearanceApp, tx| {
                        tx.copy(hint_copy.clone());
                        s.push_toast(
                            "copied-cmd",
                            ToastPayload {
                                title: "Copied Rebuild Command".into(),
                                description: format!("`{hint_copy}` copied to clipboard"),
                                undo_sweep_id: None,
                            },
                            tx.clock(),
                            tx,
                        );
                    })
                    .build(k)
                    .key("insp-copy-cmd"),
            );
        }
        body = body.child(rebuild_card);
    }

    // Include Switch
    if it.is_selectable() {
        body = body.child(
            row()
                .w_full()
                .padding(12.0)
                .radius(9.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .center_y()
                .justify_between()
                .child(
                    text("Stage for Sweep")
                        .font(tabular_ui_medium(k))
                        .color(c.text_base),
                )
                .child(
                    Switch::new(it.selected)
                        .label("Stage for Sweep")
                        .hide_label()
                        .on_toggle_tx(move |s: &mut ClearanceApp, tx| {
                            s.toggle_item_by_id(&item_id);
                            tx.instant();
                        })
                        .build(k)
                        .key("insp-stage-switch"),
                ),
        );
    }

    body
}

// ===========================================================================
// Quarantine Vault & Safety Constitution Views
// ===========================================================================

fn build_vault_view(s: &ClearanceApp, k: &Kit) -> El {
    let c = &k.c;
    let manifests: Vec<VaultManifest> = if s.dataset == DatasetMode::LiveMac {
        s.real_vault.clone()
    } else {
        s.synthetic_vault.iter().map(|sv| sv.manifest.clone()).collect()
    };

    let mut content = col()
        .key("vault-scroll")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .padding(22.0)
        .gap(14.0)
        .child(
            col()
                .w_full()
                .padding(16.0)
                .gap(6.0)
                .radius(12.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(
                    row()
                        .center_y()
                        .gap(8.0)
                        .child(icon(&ICON_VAULT, 18.0).color(oklch(0.72, 0.15, 152.0)))
                        .child(
                            text("Reversible Quarantine Vault (~/.clearance-vault)")
                                .font(tabular_ui_medium(k))
                                .color(c.text_base),
                        ),
                )
                .child(
                    text("When sweeping in Vault mode, Cleanse atomically moves items into ~/.clearance-vault/<sweep-id>/ with a JSON manifest instead of unlinking inodes. Test your projects and apps freely — 1-click Restore puts every file back in its exact original path.")
                        .font(tabular_small(k))
                        .color(c.text_muted)
                        .wrap(),
                ),
        );

    if manifests.is_empty() {
        content = content.child(
            col()
                .w_full()
                .padding(32.0)
                .center()
                .gap(6.0)
                .radius(12.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(
                    text("Quarantine Vault is Empty")
                        .font(tabular_ui_medium(k))
                        .color(c.text_base),
                )
                .child(
                    text("Sweeps staged with 'Vault (Undo)' appear here so you can restore them anytime.")
                        .font(tabular_small(k))
                        .color(c.text_muted),
                ),
        );
    } else {
        for m in manifests {
            let sid_restore = m.sweep_id.clone();
            let sid_trash = m.sweep_id.clone();
            let is_live = s.dataset == DatasetMode::LiveMac;

            let mut card = col()
                .key(format!("vault-card-{}", m.sweep_id))
                .w_full()
                .padding(16.0)
                .gap(10.0)
                .radius(12.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(
                    row()
                        .w_full()
                        .center_y()
                        .gap(10.0)
                        .child(
                            col()
                                .grow(1.0)
                                .shrink()
                                .gap(2.0)
                                .child(
                                    text(m.label.clone())
                                        .font(tabular_ui_medium(k))
                                        .color(c.text_base),
                                )
                                .child(
                                    text(format!(
                                        "Sweep ID: {} · {}",
                                        m.sweep_id,
                                        pluralize(m.entries.len(), "entry", "entries")
                                    ))
                                    .font(tabular_small(k))
                                    .color(c.text_faint),
                                ),
                        )
                        .child(
                            Button::new("Undo & Restore All")
                                .variant(ButtonVariant::Contrast)
                                .control_size(ControlSize::Regular)
                                .icon(&ICON_UNDO)
                                .on_press_tx(move |s: &mut ClearanceApp, tx| {
                                    s.undo_sweep(&sid_restore, tx);
                                })
                                .build(k)
                                .key(format!("btn-restore-{}", m.sweep_id)),
                        )
                        .child(
                            Button::new("Move to Trash")
                                .variant(ButtonVariant::Outline)
                                .control_size(ControlSize::Regular)
                                .on_press_tx(move |s: &mut ClearanceApp, tx| {
                                    if is_live {
                                        if let Ok(n) = move_vault_sweep_to_trash(&sid_trash) {
                                            s.real_vault = load_vault_manifests();
                                            s.push_toast(
                                                "vault-trashed",
                                                ToastPayload {
                                                    title: format!("Moved Vault Sweep to {}", platform::TRASH_NAME),
                                                    description: format!(
                                                        "Moved {} to the {}",
                                                        pluralize(n, "item", "items"),
                                                        platform::TRASH_NAME
                                                    ),
                                                    undo_sweep_id: None,
                                                },
                                                tx.clock(),
                                                tx,
                                            );
                                        }
                                    } else {
                                        s.synthetic_vault
                                            .retain(|sv| sv.manifest.sweep_id != sid_trash);
                                    }
                                    tx.animated();
                                })
                                .build(k)
                                .key(format!("btn-trash-{}", m.sweep_id)),
                        ),
                );

            for e in m.entries.iter().take(6) {
                card = card.child(
                    row()
                        .w_full()
                        .center_y()
                        .justify_between()
                        .gap(12.0)
                        .child(
                            text(e.original_path.to_string_lossy().into_owned())
                                .font(tabular_small(k))
                                .color(c.text_muted)
                                .truncate_middle(),
                        )
                        .child(
                            text(format_bytes(e.bytes))
                                .font(tabular_small_medium(k))
                                .color(c.text_base),
                        ),
                );
            }
            content = content.child(card);
        }
    }

    content
}

fn build_constitution_view(k: &Kit) -> El {
    let c = &k.c;
    let rules = [
        (
            "Layer 1 · Immutable System & OS Prefix Barrier",
            "Unconditionally rejects /System, /usr, /bin, /sbin, /etc, /var, /private, /Library, /Applications, /opt/homebrew/Cellar, and /Volumes.",
        ),
        (
            "Layer 2 · Personal Data, Credentials & iCloud Vault Shield",
            "Hard-blocks ~/.ssh, ~/.gnupg, ~/.aws, ~/Library/Keychains, Passwords, Mail, Messages, Photos, Safari, Preferences, Mobile Documents (iCloud), and all .git repositories.",
        ),
        (
            "Layer 3 · Symlink Escape & Traversal Immunity",
            "Lexically normalizes every path and checks symlink_metadata() so a symlink inside a cache folder can never redirect a sweep to personal files.",
        ),
        (
            "Layer 4 · Build Sentinel & Running-Process Proof",
            "A target/ folder is only eligible if sibling Cargo.toml AND Cargo build markers exist. App caches owned by a currently running program are automatically paused.",
        ),
        (
            "Layer 5 · Zero-rm Reclamation (Quarantine Vault + system Trash)",
            "Cleanse never executes raw rm -rf on user paths. Sweeps move items into ~/.clearance-vault with 1-click Undo or into the system Trash (Recycle Bin on Windows).",
        ),
    ];

    let mut page = col()
        .key("safety-constitution-scroll")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .padding(24.0)
        .gap(12.0)
        .child(
            col()
                .w_full()
                .gap(4.0)
                .child(
                    text("5-Layer Constitutional Safety Architecture")
                        .font(tabular_ui_medium(k))
                        .color(c.text_base),
                )
                .child(
                    text("Every candidate item is re-verified against all five barriers immediately before any file move occurs.")
                        .font(tabular_small(k))
                        .color(c.text_muted),
                ),
        );

    for (idx, (heading, body)) in rules.iter().enumerate() {
        page = page.child(
            col()
                .key(format!("rule-card-{idx}"))
                .w_full()
                .padding(14.0)
                .gap(4.0)
                .radius(10.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(
                    row()
                        .center_y()
                        .gap(8.0)
                        .child(icon(&ICON_SHIELD, 15.0).color(oklch(0.72, 0.15, 152.0)))
                        .child(
                            text(*heading)
                                .font(tabular_ui_medium(k))
                                .color(c.text_base),
                        ),
                )
                .child(
                    text(*body)
                        .font(tabular_small(k))
                        .color(c.text_muted)
                        .wrap(),
                ),
        );
    }

    page
}

// ===========================================================================
// Overlays: Sonner ToastStack, Instant 0ms ⌘K Palette, and Dev/Stress Harness
// ===========================================================================

fn build_toast_overlay(s: &ClearanceApp, k: &Kit) -> Option<El> {
    if s.toasts.entries().is_empty() {
        return None;
    }

    let mut region = ToastRegion::new();
    for entry in s.toasts.entries().iter().rev().take(3) {
        let id = entry.id;
        let mut t = Toast::new(format!("toast-{id}"), entry.options.title.clone())
            .description(entry.options.description.clone())
            .lead(icon(&ICON_SHIELD, 16.0).color(oklch(0.72, 0.15, 152.0)))
            .on_close(move |s: &mut ClearanceApp| {
                let _ = s.toasts.step(ToastEvent::Dismissed {
                    id: Some(id),
                    now: 0.0,
                });
            });

        if let Some(sid) = entry.options.undo_sweep_id.clone() {
            t = t.action(k, "Undo Sweep", true, move |s: &mut ClearanceApp, tx| {
                s.undo_sweep(&sid, tx);
                let _ = s.toasts.step(ToastEvent::Dismissed {
                    id: Some(id),
                    now: tx.clock(),
                });
            });
        }
        region = region.toast(t.build(k));
    }

    Some(
        region
            .build(k)
            .on_hover_change(|s: &mut ClearanceApp, inside, tx| {
                let ev = if inside {
                    ToastEvent::PointerOver { now: tx.clock() }
                } else {
                    ToastEvent::PointerLeft { now: tx.clock() }
                };
                for eff in s.toasts.step(ev) {
                    if let ToastEffect::WakeAt(due) = eff {
                        tx.wake_at(due);
                    }
                }
            }),
    )
}

/// Instant `0ms` `⌘K` Command Palette (`remove-animations`: `.instant_changes()`, zero enter/exit curve).
fn build_command_palette(s: &ClearanceApp, k: &Kit) -> El {
    let c = &k.c;
    let cmds = s.filtered_palette_commands();
    let cursor = s.palette_cursor.min(cmds.len().saturating_sub(1));

    let mut list = col()
        .key("cmdk-results")
        .w_full()
        .max_h(310.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .padding(6.0)
        .gap(2.0);

    for (idx, cmd) in cmds.into_iter().enumerate() {
        let active = idx == cursor;
        let act = cmd.action.clone();
        let mut r = row()
            .key(cmd.id)
            .w_full()
            .px(10.0)
            .py(7.0)
            .gap(10.0)
            .radius(7.0)
            .center_y()
            .fill(if active {
                k.alpha(c.text_accent, if k.dark { 0.22 } else { 0.14 })
            } else {
                rune::Color::TRANSPARENT
            })
            .hover(c.hover)
            .on_press_tx(move |s: &mut ClearanceApp, tx| {
                s.execute_palette_action(act.clone(), tx);
            })
            .child(
                col()
                    .grow(1.0)
                    .shrink()
                    .gap(1.0)
                    .child(
                        text(cmd.title)
                            .font(tabular_ui_medium(k))
                            .color(c.text_base)
                            .truncate(),
                    )
                    .child(
                        text(cmd.subtitle)
                            .font(tabular_small(k))
                            .color(c.text_muted)
                            .truncate(),
                    ),
            );
        if let Some(sc) = cmd.shortcut {
            r = r.child(
                text(platform::kbd(sc))
                    .font(tabular_small_medium(k))
                    .color(c.text_faint),
            );
        }
        list = list.child(r);
    }

    let modal_box = col()
        .key("cmdk-dialog")
        .w(540.0)
        .radius(12.0)
        .clip_rounded(12.0)
        .fill(c.bg_base)
        .border(1.0, c.border_strong)
        .shadow(16.0, 40.0, oklcha(0.0, 0.0, 0.0, 0.42))
        .instant_changes()
        .motion(Spring::NONE)
        .on_background_press(|_: &mut ClearanceApp| {})
        .child(
            row()
                .w_full()
                .padding(10.0)
                .border_sides([0.0, 0.0, 1.0, 0.0], c.border_weak)
                .child(
                    TextInput::new(&s.palette_query)
                        .placeholder("Type a command, category, or stress dataset…")
                        .leading_icon(&glyphs::SEARCH)
                        .plain()
                        .width(None)
                        .on_edit(|s: &mut ClearanceApp| &mut s.palette_query)
                        .on_change(|s: &mut ClearanceApp, tx| {
                            s.palette_cursor = 0;
                            tx.instant();
                        })
                        .on_key(handle_palette_key)
                        .build(k)
                        .key("cmdk-input"),
                ),
        )
        .child(list);

    col()
        .key("cmdk-scrim")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .pt(88.0)
        .items_center()
        .fill(oklcha(0.0, 0.0, 0.0, if k.dark { 0.48 } else { 0.24 }))
        .instant_changes()
        .motion(Spring::NONE)
        .on_background_press_tx(|s: &mut ClearanceApp, tx| {
            s.palette_open = false;
            tx.instant();
        })
        .child(modal_box)
}

/// Interactive `prototype` + `break-ui` Dev Harness bar (`⌘D`).
fn build_dev_harness_overlay(s: &ClearanceApp, k: &Kit) -> El {
    let ds_idx = DatasetMode::ALL.iter().position(|&d| d == s.dataset);
    let ds_labels: Vec<&str> = DatasetMode::ALL.iter().map(|d| d.label()).collect();

    row()
        .key("dev-harness-bar")
        .pin(Some(48.0), None, None, Some(84.0))
        .padding(8.0)
        .px(12.0)
        .gap(10.0)
        .center_y()
        .radius(10.0)
        .backdrop_blur(12.0)
        .fill(if k.dark {
            oklcha(0.16, 0.02, 255.0, 0.92)
        } else {
            oklcha(0.98, 0.01, 255.0, 0.94)
        })
        .border(1.0, k.c.border_strong)
        .shadow(8.0, 24.0, oklcha(0.0, 0.0, 0.0, 0.30))
        .child(
            text("break-ui Dataset")
                .font(tabular_small_medium(k))
                .color(k.c.text_muted),
        )
        .child(
            SegmentedControl::new(&ds_labels, ds_idx)
                .label("Stress Test Dataset")
                .fit()
                .on_pick(|s: &mut ClearanceApp, idx, tx| {
                    s.switch_dataset(DatasetMode::ALL[idx]);
                    tx.instant();
                })
                .build(k)
                .key("harness-ds-seg"),
        )
        .child(
            IconButton::new(&glyphs::CLOSE)
                .variant(IconButtonVariant::GhostMuted)
                .help(platform::kbd("Close Harness (⌘D)"))
                .on_press_tx(|s: &mut ClearanceApp, tx| {
                    s.dev_harness_open = false;
                    tx.instant();
                })
                .build(k)
                .key("harness-close"),
        )
}
