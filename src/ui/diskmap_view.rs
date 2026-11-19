//! Disk Map surface (`ui/diskmap_view.rs`): where every byte on the Mac lives, and what can go.
//!
//! Four lenses on one scan: **Explorer** (drill from `/` down), **Can Delete** (a flat,
//! non-overlapping list of everything that is safe or probably safe to remove), **Biggest Files**
//! and **By Type**. Every row carries a plain-English verdict and the reason behind it.

use std::path::{Path, PathBuf};

use rune::{Color, El, ScrollSpec, col, el, icon, oklcha, row, spacer, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, SegmentedControl, glyphs};

use super::{ClearanceApp, NavRoute, ToastPayload};
use crate::cleaner::reveal_in_finder;
use crate::diskmap::{Candidate, Class, Node, NodeKind, TopFile, duplicate_savings, now_secs};
use crate::platform;
use crate::model::{format_age, format_bytes, format_count, pluralize};
use crate::ui::theme::{
    ICON_FILE, ICON_FOLDER, ICON_LAYERS, ICON_SHIELD, class_accent, class_colors, tabular_small,
    tabular_small_medium, tabular_ui_medium,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapTab {
    Explorer,
    CanDelete,
    BiggestFiles,
    ByType,
}

impl MapTab {
    const ALL: [MapTab; 4] = [MapTab::Explorer, MapTab::CanDelete, MapTab::BiggestFiles, MapTab::ByType];
    const LABELS: [&'static str; 4] = ["Explorer", "Can Delete", "Biggest Files", "By Type"];
}

/// What the inspector shows: a snapshot of whichever row was last clicked.
#[derive(Clone, Debug)]
pub struct Sel {
    pub name: String,
    pub path: PathBuf,
    pub kind: NodeKind,
    pub bytes: u64,
    pub logical: u64,
    pub files: u64,
    pub mtime: u64,
    pub class: Class,
    pub reason: &'static str,
    pub how: &'static str,
    pub by_class: [u64; 4],
    pub unreadable: bool,
}

impl Sel {
    pub fn from_node(n: &Node) -> Self {
        Self {
            name: n.name.clone(),
            path: n.path.clone(),
            kind: n.kind,
            bytes: n.bytes,
            logical: n.logical,
            files: n.files,
            mtime: n.newest_mtime,
            class: n.verdict.class,
            reason: n.verdict.reason,
            how: n.verdict.how,
            by_class: n.by_class,
            unreadable: n.unreadable,
        }
    }

    pub fn from_candidate(c: &Candidate) -> Self {
        let mut by_class = [0; 4];
        by_class[c.class.idx()] = c.bytes;
        Self {
            name: c.name.clone(),
            path: c.path.clone(),
            kind: NodeKind::Dir,
            bytes: c.bytes,
            logical: c.bytes,
            files: c.files,
            mtime: c.newest_mtime,
            class: c.class,
            reason: c.reason,
            how: c.how,
            by_class,
            unreadable: false,
        }
    }

    fn from_top(f: &TopFile) -> Self {
        let mut by_class = [0; 4];
        by_class[f.class.idx()] = f.bytes;
        Self {
            name: f.name.clone(),
            path: f.path.clone(),
            kind: NodeKind::File,
            bytes: f.bytes,
            logical: f.logical,
            files: 1,
            mtime: f.mtime,
            class: f.class,
            reason: f.reason,
            how: f.how,
            by_class,
            unreadable: false,
        }
    }
}

pub struct MapUi {
    /// Child names from the root down to the folder being viewed.
    pub nav: Vec<String>,
    pub selected: Option<Sel>,
    pub tab: MapTab,
    /// Only show things that contain this class (set by clicking a legend chip).
    pub filter: Option<Class>,
}

impl MapUi {
    pub fn new() -> Self {
        Self {
            nav: Vec::new(),
            selected: None,
            tab: MapTab::Explorer,
            filter: None,
        }
    }
}

impl Default for MapUi {
    fn default() -> Self {
        Self::new()
    }
}

impl ClearanceApp {
    /// Jump the Explorer to the folder that contains `path`.
    fn map_show_in_explorer(&mut self, path: &Path) {
        let mut nav: Vec<String> = path
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => s.to_str().map(str::to_owned),
                _ => None,
            })
            .collect();
        nav.pop(); // the parent folder, so the item itself is visible in the list
        // Walk as far as the (pruned) tree reaches.
        let mut ok = Vec::new();
        if let Some(mut cur) = self.map.root.as_ref() {
            for name in nav {
                match cur.find_child(&name) {
                    Some(next) if next.is_dir() => {
                        ok.push(name);
                        cur = next;
                    }
                    _ => break,
                }
            }
        }
        self.map_ui.nav = ok;
        self.map_ui.tab = MapTab::Explorer;
        self.map_ui.filter = None;
        self.route = NavRoute::DiskMap;
    }

    fn map_open_folder(&mut self, name: &str) {
        self.map_ui.nav.push(name.to_owned());
        self.map_ui.filter = None;
        self.map_ui.selected = self.map.dir_at(&self.map_ui.nav).map(Sel::from_node);
    }

    pub fn map_go_up(&mut self) -> bool {
        if self.map_ui.nav.is_empty() {
            return false;
        }
        self.map_ui.nav.pop();
        self.map_ui.selected = self.map.dir_at(&self.map_ui.nav).map(Sel::from_node);
        true
    }

    fn map_crumb_to(&mut self, depth: usize) {
        self.map_ui.nav.truncate(depth);
        self.map_ui.filter = None;
        self.map_ui.selected = self.map.dir_at(&self.map_ui.nav).map(Sel::from_node);
    }

    fn finding_for_path(&self, path: &Path) -> Option<String> {
        self.items.iter().find(|i| i.path == path).map(|i| i.id.clone())
    }
}

// ---------------------------------------------------------------------------
// Small pieces
// ---------------------------------------------------------------------------

pub fn class_pill(class: Class, k: &Kit) -> El {
    let (bg, fg, border) = class_colors(class, k.dark);
    let mut r = row()
        .h(20.0)
        .px(7.0)
        .gap(4.0)
        .radius(5.0)
        .center_y()
        .fill(bg)
        .border(1.0, border);
    if class == Class::Required {
        r = r.child(icon(&glyphs::LOCK, 11.0).color(fg));
    }
    r.child(text(class.label()).font(tabular_small_medium(k)).color(fg))
}

fn size_bar(frac: f32, color: Color, track: Color) -> El {
    let w = 84.0_f32;
    let fw = (w * frac.clamp(0.0, 1.0)).max(if frac > 0.0 { 2.0 } else { 0.0 });
    row()
        .size(w, 6.0)
        .radius(3.0)
        .clip_rounded(3.0)
        .fill(track)
        .child(el().size(fw, 6.0).fill(color))
}

fn fmt_pct(frac: f64) -> String {
    let p = frac * 100.0;
    if p < 0.1 {
        "<0.1%".into()
    } else if p < 10.0 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

fn changed_text(mtime: u64, now: u64) -> String {
    if mtime == 0 {
        return String::new();
    }
    let days = if now > mtime { ((now - mtime) / 86_400) as u32 } else { 0 };
    if days == 0 {
        "changed today".into()
    } else {
        format!("changed {}", format_age(days))
    }
}

fn parent_display(path: &Path) -> String {
    path.parent().map_or(String::new(), |p| p.to_string_lossy().into_owned())
}

fn card() -> El {
    col().w_full()
}

fn track_color(k: &Kit) -> Color {
    if k.dark {
        oklcha(0.95, 0.0, 0.0, 0.08)
    } else {
        oklcha(0.20, 0.0, 0.0, 0.07)
    }
}

fn is_command(how: &str) -> bool {
    let first = how.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "brew" | "docker" | "npm" | "pnpm" | "bun" | "cargo" | "rustup" | "xcrun" | "xcode-select" | "git" | "ollama"
            | "pod" | "swift" | "tmutil" | "diskutil" | "uv" | "pip" | "ollama:"
    )
}

/// The runnable part of a hint like `docker system prune -a   (removes unused…)`.
fn command_part(how: &str) -> &str {
    how.split("   ").next().unwrap_or(how).trim()
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

pub fn build_map_view(s: &ClearanceApp, k: &Kit) -> El {
    let c = &k.c;
    let mut page = col()
        .key("diskmap-scroll")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .padding(22.0)
        .gap(14.0)
        .child(build_hero(s, k));

    if let Some(callout) = build_hidden_callout(s, k) {
        page = page.child(callout);
    }

    let idx = MapTab::ALL.iter().position(|t| *t == s.map_ui.tab);
    page = page.child(
        SegmentedControl::new(&MapTab::LABELS, idx)
            .label("Disk Map view")
            .fit()
            .on_pick(|s: &mut ClearanceApp, i, tx| {
                s.map_ui.tab = MapTab::ALL[i];
                tx.instant();
            })
            .build(k)
            .key("map-tab-switcher"),
    );

    if s.map.root.is_none() {
        return page.child(
            card()
                .padding(32.0)
                .center()
                .gap(6.0)
                .radius(12.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(
                    text(if s.map.scanning {
                        "Mapping your Mac…"
                    } else {
                        "No scan yet"
                    })
                    .font(tabular_ui_medium(k))
                    .color(c.text_base),
                )
                .child(
                    text(if s.map.scanning {
                        "The first folders appear here as soon as each one finishes.".to_owned()
                    } else {
                        format!("Press Scan {} ({}) to map every file on this disk.", platform::DEVICE, platform::kbd("⌘R"))
                    })
                    .font(tabular_small(k))
                    .color(c.text_muted),
                ),
        );
    }

    match s.map_ui.tab {
        MapTab::Explorer => page.child(build_explorer(s, k)),
        MapTab::CanDelete => page.child(build_can_delete(s, k)),
        MapTab::BiggestFiles => page.child(build_biggest_files(s, k)),
        MapTab::ByType => page.child(build_by_type(s, k)),
    }
}

fn build_hero(s: &ClearanceApp, k: &Kit) -> El {
    let c = &k.c;
    let used = s.disk.used_bytes();
    let free = s.disk.free_bytes.min(s.disk.total_bytes);
    let root = s.map.root.as_ref();
    let seen = root.map_or(0, |r| r.bytes);
    let by = root.map_or([0u64; 4], |r| r.by_class);
    // Used but not attributable to any file we could read.
    let hidden = used.saturating_sub(seen);
    let total = s.disk.total_bytes.max(1) as f64;
    let frac = |b: u64| (b as f64 / total) as f32;

    let status: El = if s.map.scanning {
        col()
            .gap(2.0)
            .child(
                text(format!(
                    "{} · {} files · {}",
                    if s.map.progress.phase.is_empty() {
                        "Scanning"
                    } else {
                        &s.map.progress.phase
                    },
                    format_count(s.map.progress.files as usize),
                    format_bytes(s.map.progress.bytes)
                ))
                .font(tabular_small_medium(k))
                .color(c.text_base)
                .truncate(),
            )
            .child(
                text("Folders appear below as each one finishes.")
                    .font(tabular_small(k))
                    .color(c.text_faint),
            )
    } else if let Some(ms) = s.map.elapsed_ms {
        col().gap(2.0).child(
            text(format!(
                "{} files scanned in {:.0}s",
                format_count(s.map.files_scanned as usize),
                ms as f64 / 1000.0
            ))
            .font(tabular_small(k))
            .color(c.text_muted),
        )
    } else {
        col()
    };

    let mut bar = row()
        .key("map-class-bar")
        .w_full()
        .h(14.0)
        .radius(7.0)
        .clip_rounded(7.0)
        .gap(1.5)
        .fill(track_color(k));
    for class in Class::ALL {
        let b = by[class.idx()];
        if b > 0 {
            bar = bar.child(
                el().key(format!("map-seg-{}", class.short()))
                    .grow(frac(b).max(0.004))
                    .h_full()
                    .fill(class_accent(class, k.dark)),
            );
        }
    }
    if hidden > 0 {
        bar = bar.child(
            el().key("map-seg-hidden")
                .grow(frac(hidden).max(0.004))
                .h_full()
                .fill(oklcha(0.55, 0.02, 265.0, if k.dark { 0.45 } else { 0.35 })),
        );
    }
    bar = bar.child(el().key("map-seg-free").grow(frac(free).max(0.01)).h_full());

    let mut legend = row().w_full().gap(8.0).wrap();
    for class in Class::ALL {
        let b = by[class.idx()];
        let active = s.map_ui.filter == Some(class);
        let accent = class_accent(class, k.dark);
        legend = legend.child(
            row()
                .key(format!("map-chip-{}", class.short()))
                .px(10.0)
                .py(6.0)
                .gap(7.0)
                .radius(8.0)
                .center_y()
                .fill(if active {
                    k.alpha(accent, if k.dark { 0.24 } else { 0.16 })
                } else {
                    c.layer_01
                })
                .border(1.0, if active { accent } else { c.border_weak })
                .hover(c.hover)
                .on_press_tx(move |s: &mut ClearanceApp, tx| {
                    s.map_ui.filter = if s.map_ui.filter == Some(class) { None } else { Some(class) };
                    s.map_ui.tab = MapTab::Explorer;
                    tx.instant();
                })
                .child(el().size(8.0, 8.0).radius(4.0).fill(accent))
                .child(
                    col()
                        .gap(0.0)
                        .child(
                            text(format_bytes(b))
                                .font(tabular_ui_medium(k))
                                .color(c.text_base),
                        )
                        .child(
                            text(class.label())
                                .font(tabular_small(k))
                                .color(c.text_muted),
                        ),
                ),
        );
    }
    if hidden > 0 {
        legend = legend.child(
            row()
                .px(10.0)
                .py(6.0)
                .gap(7.0)
                .radius(8.0)
                .center_y()
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(
                    el().size(8.0, 8.0)
                        .radius(4.0)
                        .fill(oklcha(0.55, 0.02, 265.0, if k.dark { 0.7 } else { 0.6 })),
                )
                .child(
                    col()
                        .gap(0.0)
                        .child(
                            text(format_bytes(hidden))
                                .font(tabular_ui_medium(k))
                                .color(c.text_base),
                        )
                        .child(
                            text(format!("Held by {}", platform::OS_NAME))
                                .font(tabular_small(k))
                                .color(c.text_muted),
                        ),
                ),
        );
    }

    card()
        .key("map-hero")
        .padding(18.0)
        .gap(14.0)
        .radius(14.0)
        .fill(c.layer_01)
        .border(1.0, c.border_weak)
        .child(
            row()
                .w_full()
                .gap(16.0)
                .center_y()
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .gap(2.0)
                        .child(
                            text("WHERE YOUR STORAGE WENT")
                                .font(tabular_small_medium(k))
                                .color(c.text_faint),
                        )
                        .child(
                            row()
                                .gap(8.0)
                                .center_y()
                                .child(
                                    text(format_bytes(used))
                                        .font(crate::ui::theme::hero_metric_font(k))
                                        .color(c.text_base),
                                )
                                .child(
                                    text(format!(
                                        "used of {} · {} free",
                                        format_bytes(s.disk.total_bytes),
                                        format_bytes(free)
                                    ))
                                    .font(tabular_ui_medium(k))
                                    .color(c.text_muted),
                                ),
                        ),
                )
                .child(status),
        )
        .child(bar)
        .child(legend)
}

/// Explains the gap between "used" and "files we can see". This is often the biggest single item.
fn build_hidden_callout(s: &ClearanceApp, k: &Kit) -> Option<El> {
    let c = &k.c;
    let root = s.map.root.as_ref()?;
    if s.map.scanning {
        return None;
    }
    let hidden = s.disk.used_bytes().saturating_sub(root.bytes);
    let blocked = s.map.denied_count;
    if hidden < 1_000_000_000 && blocked == 0 {
        return None;
    }

    let snaps = s.map.snapshots.len();
    let pending_update = s.map.snapshots.iter().any(|n| n.contains("MSUPrepareUpdate"));
    let mut body = String::new();
    if hidden >= 1_000_000_000 && platform::IS_WINDOWS {
        body.push_str(
            "Windows keeps space the scan can't count: System Restore points, Windows Update files, the hibernation and page files, and other protected system data. Settings → System → Storage → Temporary files shows what Windows can safely clear.",
        );
    } else if hidden >= 1_000_000_000 {
        body.push_str(
            "Mostly APFS snapshots: macOS keeps old copies of files so updates and Time Machine can roll back, and deleted files stay pinned until the snapshot goes. The Preboot, Recovery and VM volumes count here too. ",
        );
        if snaps > 0 {
            body.push_str(&format!(
                "{} found on this Mac{}. ",
                pluralize(snaps, "local snapshot", "local snapshots"),
                if pending_update {
                    ", including a staged macOS update"
                } else {
                    ""
                }
            ));
        }
        body.push_str("macOS frees this itself when it needs room. Finishing a pending update and restarting usually releases the most.");
    }
    if blocked > 0 {
        if !body.is_empty() {
            body.push(' ');
        }
        body.push_str(&format!(
            "{} blocked by {}. {}",
            pluralize(blocked as usize, "folder", "folders"),
            platform::OS_NAME,
            if platform::IS_WINDOWS {
                "Run Cleanse as administrator to include them."
            } else {
                "Grant Full Disk Access to include them."
            }
        ));
    }

    let mut actions = row().gap(8.0).wrap();
    if blocked > 0 || (platform::IS_WINDOWS && hidden >= 1_000_000_000) {
        let label = if platform::IS_WINDOWS {
            "Open Storage settings"
        } else {
            "Open Full Disk Access settings"
        };
        actions = actions.child(
            Button::new(label)
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Regular)
                .on_press(|_: &mut ClearanceApp| {
                    if !rune::is_headless() {
                        let _ = if platform::IS_WINDOWS {
                            std::process::Command::new("explorer").arg("ms-settings:storagesense").spawn()
                        } else {
                            std::process::Command::new("open")
                                .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
                                .spawn()
                        };
                    }
                })
                .build(k)
                .key("map-open-fda"),
        );
    }
    if snaps > 0 {
        actions = actions.child(
            Button::new("Copy snapshot list command")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Regular)
                .icon(&glyphs::COPY)
                .on_press_tx(|s: &mut ClearanceApp, tx| {
                    let cmd = "tmutil listlocalsnapshots /System/Volumes/Data";
                    tx.copy(cmd.to_owned());
                    s.push_toast(
                        "copied-cmd",
                        ToastPayload {
                            title: "Copied command".into(),
                            description: format!("`{cmd}` copied to clipboard"),
                            undo_sweep_id: None,
                        },
                        tx.clock(),
                        tx,
                    );
                })
                .build(k)
                .key("map-copy-snap"),
        );
    }

    Some(
        card()
            .key("map-hidden-callout")
            .padding(16.0)
            .gap(8.0)
            .radius(12.0)
            .fill(oklcha(0.62, 0.05, 265.0, if k.dark { 0.14 } else { 0.10 }))
            .border(1.0, c.border_weak)
            .child(
                row()
                    .gap(8.0)
                    .center_y()
                    .child(icon(&ICON_LAYERS, 16.0).color(c.text_muted))
                    .child(
                        text(if hidden >= 1_000_000_000 {
                            format!("{} is used, but not by any file you can see", format_bytes(hidden))
                        } else {
                            "Some folders are hidden from the scan".to_owned()
                        })
                        .font(tabular_ui_medium(k))
                        .color(c.text_base),
                    ),
            )
            .child(text(body).font(tabular_small(k)).color(c.text_muted).wrap())
            .child(actions),
    )
}

// ---------------------------------------------------------------------------
// Explorer
// ---------------------------------------------------------------------------

fn build_explorer(s: &ClearanceApp, k: &Kit) -> El {
    let c = &k.c;
    let Some(dir) = s.map.dir_at(&s.map_ui.nav) else {
        return col();
    };
    let now = now_secs();
    let filter = s.map_ui.filter;

    // Breadcrumb: Macintosh HD › Users › trent
    let mut crumbs = row().w_full().gap(2.0).center_y().wrap();
    let depth_now = s.map_ui.nav.len().min(
        // never show more crumbs than the tree actually resolved
        {
            let mut n = 0;
            let mut cur = s.map.root.as_ref();
            for name in &s.map_ui.nav {
                match cur.and_then(|d| d.find_child(name)) {
                    Some(next) if next.is_dir() => {
                        n += 1;
                        cur = Some(next);
                    }
                    _ => break,
                }
            }
            n
        },
    );
    let mut labels: Vec<String> = vec![platform::ROOT_NAME.into()];
    labels.extend(s.map_ui.nav.iter().take(depth_now).cloned());
    let last = labels.len() - 1;
    for (i, label) in labels.into_iter().enumerate() {
        if i > 0 {
            crumbs = crumbs.child(icon(&crate::ui::theme::ICON_CHEVRON, 11.0).color(c.text_faint));
        }
        crumbs = crumbs.child(
            row()
                .key(format!("map-crumb-{i}"))
                .px(6.0)
                .py(3.0)
                .radius(6.0)
                .hover(c.hover)
                .on_press_tx(move |s: &mut ClearanceApp, tx| {
                    s.map_crumb_to(i);
                    tx.instant();
                })
                .child(
                    text(label)
                        .font(if i == last {
                            tabular_ui_medium(k)
                        } else {
                            tabular_small_medium(k)
                        })
                        .color(if i == last { c.text_base } else { c.text_muted }),
                ),
        );
    }

    let weight = |n: &Node| -> u64 { filter.map_or(n.bytes, |f| n.by_class[f.idx()]) };
    // Blocked folders have no size but are exactly what the user needs to see.
    let mut rows: Vec<&Node> = dir
        .children
        .iter()
        .filter(|n| weight(n) > 0 || (filter.is_none() && n.unreadable))
        .collect();
    if filter.is_some() {
        rows.sort_by_key(|n| std::cmp::Reverse(weight(n)));
    }
    let total = filter.map_or(dir.bytes, |f| dir.by_class[f.idx()]).max(1);

    let mut list = col().key("map-explorer-rows").w_full().gap(2.0);
    if let Some(f) = filter {
        list = list.child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .px(4.0)
                .py(4.0)
                .child(class_pill(f, k))
                .child(
                    text(format!(
                        "{} in this folder. Showing only what contains it.",
                        format_bytes(dir.by_class[f.idx()])
                    ))
                    .font(tabular_small(k))
                    .color(c.text_muted),
                )
                .child(spacer())
                .child(
                    Button::new("Show all")
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Regular)
                        .on_press(|s: &mut ClearanceApp| s.map_ui.filter = None)
                        .build(k)
                        .key("map-clear-filter"),
                ),
        );
    }
    if rows.is_empty() {
        list = list.child(
            text(if dir.children.is_empty() {
                "Nothing big in here. Everything inside is smaller than 64 KB per file."
            } else {
                "Nothing in this folder matches."
            })
            .font(tabular_small(k))
            .color(c.text_muted)
            .padding(14.0),
        );
    }

    let selected_path = s.map_ui.selected.as_ref().map(|x| (&x.path, x.kind));
    for n in rows {
        let bytes = weight(n);
        let frac = bytes as f64 / total as f64;
        let name = n.name.clone();
        let is_dir = n.kind == NodeKind::Dir;
        let sel = Sel::from_node(n);
        let accent = class_accent(n.verdict.class, k.dark);
        let active = selected_path.is_some_and(|(p, kd)| p == &n.path && kd == n.kind);

        let subline = match n.kind {
            NodeKind::Rollup => "Too small to list one by one. Each item is under 512 KB.".to_owned(),
            _ if n.unreadable => format!("{} blocked this folder. {}", platform::OS_NAME, platform::ACCESS_FIX),
            _ => {
                let when = changed_text(n.newest_mtime, now);
                if when.is_empty() {
                    n.verdict.reason.to_owned()
                } else {
                    format!("{} · {}", n.verdict.reason, when)
                }
            }
        };

        let mut r = row()
            .key(format!("map-row-{}-{}", n.path.display(), n.name))
            .w_full()
            .px(12.0)
            .py(8.0)
            .gap(12.0)
            .radius(8.0)
            .center_y()
            .fill(if active {
                k.alpha(c.text_accent, if k.dark { 0.18 } else { 0.11 })
            } else {
                Color::TRANSPARENT
            })
            .hover(c.hover)
            .child(
                icon(
                    match n.kind {
                        NodeKind::Dir => &ICON_FOLDER,
                        NodeKind::File => &ICON_FILE,
                        NodeKind::Rollup => &ICON_LAYERS,
                    },
                    16.0,
                )
                .color(if is_dir { accent } else { c.text_muted }),
            )
            .child(
                col()
                    .grow(1.0)
                    .shrink()
                    .min_w(0.0)
                    .gap(2.0)
                    .child(
                        text(n.name.clone())
                            .font(tabular_ui_medium(k))
                            .color(c.text_base)
                            .truncate_middle(),
                    )
                    .child(
                        text(subline)
                            .font(tabular_small(k))
                            .color(c.text_muted)
                            .truncate(),
                    ),
            )
            .child(class_pill(n.verdict.class, k))
            .child(size_bar(frac as f32, accent, track_color(k)))
            .child(
                col()
                    .w(74.0)
                    .gap(1.0)
                    .child(
                        row().child(spacer()).child(
                            text(if n.unreadable && bytes == 0 {
                                "Blocked".to_owned()
                            } else {
                                format_bytes(bytes)
                            })
                            .font(tabular_ui_medium(k))
                            .color(c.text_base),
                        ),
                    )
                    .child(
                        row().child(spacer()).child(
                            text(fmt_pct(frac))
                                .font(tabular_small(k))
                                .color(c.text_faint),
                        ),
                    ),
            );
        r = if is_dir && !n.children.is_empty() {
            r.on_press_tx(move |s: &mut ClearanceApp, tx| {
                s.map_open_folder(&name);
                tx.instant();
            })
        } else {
            r.on_press_tx(move |s: &mut ClearanceApp, tx| {
                s.map_ui.selected = Some(sel.clone());
                tx.instant();
            })
        };
        list = list.child(r);
    }

    col()
        .w_full()
        .gap(8.0)
        .child(crumbs)
        .child(
            card()
                .padding(6.0)
                .radius(12.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(list),
        )
        .child(
            text("Sizes are physical bytes on disk. Folders list their largest items; the rest is combined into one row so totals always add up. Files cloned by APFS share storage and may be counted more than once.")
                .font(tabular_small(k))
                .color(c.text_faint)
                .wrap(),
        )
}

// ---------------------------------------------------------------------------
// Can Delete
// ---------------------------------------------------------------------------

fn candidate_row(cand: &Candidate, now: u64, selected: Option<&Path>, k: &Kit) -> El {
    let c = &k.c;
    let sel = Sel::from_candidate(cand);
    let accent = class_accent(cand.class, k.dark);
    let active = selected.is_some_and(|p| p == cand.path);
    let when = changed_text(cand.newest_mtime, now);
    row()
        .key(format!("cand-{}", cand.path.display()))
        .w_full()
        .px(12.0)
        .py(9.0)
        .gap(12.0)
        .radius(8.0)
        .center_y()
        .fill(if active {
            k.alpha(c.text_accent, if k.dark { 0.18 } else { 0.11 })
        } else {
            Color::TRANSPARENT
        })
        .hover(c.hover)
        .on_press_tx(move |s: &mut ClearanceApp, tx| {
            s.map_ui.selected = Some(sel.clone());
            tx.instant();
        })
        .child(el().size(4.0, 34.0).radius(2.0).fill(accent))
        .child(
            col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(2.0)
                .child(
                    text(cand.name.clone())
                        .font(tabular_ui_medium(k))
                        .color(c.text_base)
                        .truncate_middle(),
                )
                .child(
                    text(if when.is_empty() {
                        cand.reason.to_owned()
                    } else {
                        format!("{} · {}", cand.reason, when)
                    })
                    .font(tabular_small(k))
                    .color(c.text_muted)
                    .truncate(),
                )
                .child(
                    text(parent_display(&cand.path))
                        .font(tabular_small(k))
                        .color(c.text_faint)
                        .truncate_middle(),
                ),
        )
        .child(
            col()
                .w(86.0)
                .child(
                    row().child(spacer()).child(
                        text(format_bytes(cand.bytes))
                            .font(tabular_ui_medium(k))
                            .color(c.text_base),
                    ),
                )
                .child(
                    row().child(spacer()).child(
                        text(pluralize(cand.files as usize, "file", "files"))
                            .font(tabular_small(k))
                            .color(c.text_faint),
                    ),
                ),
        )
}

fn build_can_delete(s: &ClearanceApp, k: &Kit) -> El {
    let c = &k.c;
    let now = now_secs();
    let selected = s.map_ui.selected.as_ref().map(|x| x.path.as_path());
    let safe: Vec<&Candidate> = s.map.candidates.iter().filter(|x| x.class == Class::Safe).collect();
    let review: Vec<&Candidate> = s.map.candidates.iter().filter(|x| x.class == Class::Review).collect();
    let sum = |v: &[&Candidate]| v.iter().map(|x| x.bytes).sum::<u64>();

    let mut out = col().w_full().gap(14.0);
    if s.map.scanning && s.map.candidates.is_empty() {
        return out.child(
            text("The delete list is built when the scan finishes.")
                .font(tabular_small(k))
                .color(c.text_muted),
        );
    }

    let section = |class: Class, items: &[&Candidate], blurb: &str| -> El {
        let mut list = col().w_full().gap(2.0);
        for cand in items.iter().take(80) {
            list = list.child(candidate_row(cand, now, selected, k));
        }
        if items.is_empty() {
            list = list.child(
                text("Nothing found.")
                    .font(tabular_small(k))
                    .color(c.text_muted)
                    .padding(12.0),
            );
        }
        col()
            .w_full()
            .gap(8.0)
            .child(
                row()
                    .w_full()
                    .gap(10.0)
                    .center_y()
                    .child(class_pill(class, k))
                    .child(
                        text(format_bytes(items.iter().map(|x| x.bytes).sum()))
                            .font(tabular_ui_medium(k))
                            .color(c.text_base),
                    )
                    .child(
                        text(blurb.to_owned())
                            .font(tabular_small(k))
                            .color(c.text_muted)
                            .truncate(),
                    ),
            )
            .child(
                card()
                    .padding(6.0)
                    .radius(12.0)
                    .fill(c.layer_01)
                    .border(1.0, c.border_weak)
                    .child(list),
            )
    };

    out = out
        .child(
            card()
                .padding(16.0)
                .gap(4.0)
                .radius(12.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(
                    text(format!(
                        "Up to {} can be freed",
                        format_bytes(sum(&safe) + sum(&review))
                    ))
                    .font(tabular_ui_medium(k))
                    .color(c.text_base),
                )
                .child(
                    text(format!(
                        "{} regenerates by itself. {} is probably removable, but check each one first. Anything required by macOS or belonging to you is never listed.",
                        format_bytes(sum(&safe)),
                        format_bytes(sum(&review))
                    ))
                    .font(tabular_small(k))
                    .color(c.text_muted)
                    .wrap(),
                ),
        )
        .child(section(
            Class::Safe,
            &safe,
            "Rebuilds on its own. Nothing of yours is lost.",
        ))
        .child(section(
            Class::Review,
            &review,
            "Delete only if you no longer use it.",
        ));
    out
}

// ---------------------------------------------------------------------------
// Biggest files
// ---------------------------------------------------------------------------

fn build_biggest_files(s: &ClearanceApp, k: &Kit) -> El {
    let c = &k.c;
    let now = now_secs();
    if s.map.top_files.is_empty() {
        return col().child(
            text(if s.map.scanning {
                "The biggest-files list appears when the scan finishes."
            } else {
                "No files over 8 MB were found."
            })
            .font(tabular_small(k))
            .color(c.text_muted),
        );
    }
    let selected = s.map_ui.selected.as_ref().map(|x| x.path.as_path());
    let top = s.map.top_files.first().map_or(1, |f| f.bytes).max(1);
    let mut list = col().w_full().gap(2.0);
    for f in s.map.top_files.iter().take(100) {
        let sel = Sel::from_top(f);
        let accent = class_accent(f.class, k.dark);
        let active = selected.is_some_and(|p| p == f.path);
        let when = changed_text(f.mtime, now);
        list = list.child(
            row()
                .key(format!("topfile-{}", f.path.display()))
                .w_full()
                .px(12.0)
                .py(8.0)
                .gap(12.0)
                .radius(8.0)
                .center_y()
                .fill(if active {
                    k.alpha(c.text_accent, if k.dark { 0.18 } else { 0.11 })
                } else {
                    Color::TRANSPARENT
                })
                .hover(c.hover)
                .on_press_tx(move |s: &mut ClearanceApp, tx| {
                    s.map_ui.selected = Some(sel.clone());
                    tx.instant();
                })
                .child(icon(&ICON_FILE, 16.0).color(c.text_muted))
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(2.0)
                        .child(
                            text(f.name.clone())
                                .font(tabular_ui_medium(k))
                                .color(c.text_base)
                                .truncate_middle(),
                        )
                        .child(
                            text(if f.copies > 1 {
                                format!("{} copies · {}", f.copies, parent_display(&f.path))
                            } else {
                                format!("{} · {}", parent_display(&f.path), when)
                            })
                                .font(tabular_small(k))
                                .color(c.text_muted)
                                .truncate_middle(),
                        ),
                )
                .child(class_pill(f.class, k))
                .child(size_bar(f.bytes as f32 / top as f32, accent, track_color(k)))
                .child(
                    col().w(74.0).child(
                        row().child(spacer()).child(
                            text(format_bytes(f.bytes))
                                .font(tabular_ui_medium(k))
                                .color(c.text_base),
                        ),
                    ),
                ),
        );
    }
    let (dup_groups, dup_bytes) = duplicate_savings(&s.map.top_files);
    let mut out = col().w_full().gap(10.0);
    if dup_groups > 0 {
        out = out.child(
            card()
                .padding(14.0)
                .gap(4.0)
                .radius(12.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(
                    text(format!("About {} is duplicated", format_bytes(dup_bytes)))
                    .font(tabular_ui_medium(k))
                    .color(c.text_base),
                )
                .child(
                    text(format!(
                        "Found by matching name and size among your biggest files ({}). Open both copies to confirm they match, then keep one.",
                        pluralize(dup_groups, "duplicated file", "duplicated files")
                    ))
                        .font(tabular_small(k))
                        .color(c.text_muted)
                        .wrap(),
                ),
        );
    }
    out.child(
        card()
            .padding(6.0)
            .radius(12.0)
            .fill(c.layer_01)
            .border(1.0, c.border_weak)
            .child(list),
    )
}

// ---------------------------------------------------------------------------
// By type
// ---------------------------------------------------------------------------

fn build_by_type(s: &ClearanceApp, k: &Kit) -> El {
    let c = &k.c;
    if s.map.kinds.is_empty() {
        return col().child(
            text(if s.map.scanning {
                "The breakdown by type appears when the scan finishes."
            } else {
                "No file types were recorded."
            })
            .font(tabular_small(k))
            .color(c.text_muted),
        );
    }
    let mut kinds = s.map.kinds.clone();
    kinds.retain(|x| x.bytes > 0);
    kinds.sort_by_key(|x| std::cmp::Reverse(x.bytes));
    let max = kinds.first().map_or(1, |x| x.bytes).max(1);
    let total: u64 = kinds.iter().map(|x| x.bytes).sum::<u64>().max(1);

    let mut list = col().w_full().gap(2.0);
    for kind in kinds {
        let accent = if k.dark {
            rune::oklch(0.72, 0.13, kind.kind.hue())
        } else {
            rune::oklch(0.55, 0.14, kind.kind.hue())
        };
        list = list.child(
            row()
                .key(format!("kind-{}", kind.kind.label()))
                .w_full()
                .px(12.0)
                .py(10.0)
                .gap(12.0)
                .center_y()
                .child(el().size(8.0, 8.0).radius(4.0).fill(accent))
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(2.0)
                        .child(
                            text(kind.kind.label())
                                .font(tabular_ui_medium(k))
                                .color(c.text_base),
                        )
                        .child(
                            text(pluralize(kind.files as usize, "file", "files"))
                                .font(tabular_small(k))
                                .color(c.text_muted),
                        ),
                )
                .child(size_bar(kind.bytes as f32 / max as f32, accent, track_color(k)))
                .child(
                    col()
                        .w(74.0)
                        .gap(1.0)
                        .child(
                            row().child(spacer()).child(
                                text(format_bytes(kind.bytes))
                                    .font(tabular_ui_medium(k))
                                    .color(c.text_base),
                            ),
                        )
                        .child(
                            row().child(spacer()).child(
                                text(fmt_pct(kind.bytes as f64 / total as f64))
                                    .font(tabular_small(k))
                                    .color(c.text_faint),
                            ),
                        ),
                ),
        );
    }
    col()
        .w_full()
        .gap(8.0)
        .child(
            card()
                .padding(6.0)
                .radius(12.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(list),
        )
        .child(
            text("Counts every file Cleanse could read, grouped by file extension.")
                .font(tabular_small(k))
                .color(c.text_faint),
        )
}

// ---------------------------------------------------------------------------
// Inspector
// ---------------------------------------------------------------------------

pub fn build_map_inspector(s: &ClearanceApp, k: &Kit) -> El {
    let c = &k.c;
    let now = now_secs();
    let sel = s
        .map_ui
        .selected
        .clone()
        .or_else(|| s.map.dir_at(&s.map_ui.nav).map(Sel::from_node));

    let Some(sel) = sel else {
        return col()
            .key("map-inspector-empty")
            .w_full()
            .h_full()
            .padding(20.0)
            .center()
            .gap(6.0)
            .child(
                text("Nothing selected")
                    .font(tabular_ui_medium(k))
                    .color(c.text_base),
            )
            .child(
                text("Click any folder or file to see what it is, why it's there, and whether it can be deleted.")
                    .font(tabular_small(k))
                    .color(c.text_muted)
                    .wrap(),
            );
    };

    let kv = |label: &str, val: String| -> El {
        row()
            .w_full()
            .justify_between()
            .center_y()
            .gap(8.0)
            .child(text(label.to_owned()).font(tabular_small(k)).color(c.text_faint))
            .child(
                text(val)
                    .font(tabular_small_medium(k))
                    .color(c.text_base)
                    .truncate(),
            )
    };

    let used = s.disk.used_bytes().max(1);
    let path_str = sel.path.to_string_lossy().into_owned();
    let is_rollup = sel.kind == NodeKind::Rollup;

    let mut body = col()
        .key("map-inspector")
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
                .child(row().child(class_pill(sel.class, k)))
                .child(
                    text(sel.name.clone())
                        .font(tabular_ui_medium(k))
                        .color(c.text_base)
                        .wrap_anywhere(),
                )
                .child(
                    text(if sel.unreadable {
                        format!("{} blocked this folder, so its contents aren't counted. {}", platform::OS_NAME, platform::ACCESS_FIX)
                    } else if is_rollup {
                        "Everything in this folder that is too small to list on its own, combined.".to_owned()
                    } else {
                        sel.reason.to_owned()
                    })
                    .font(tabular_small(k))
                    .color(c.text_muted)
                    .wrap(),
                ),
        );

    let mut metrics = col()
        .w_full()
        .padding(12.0)
        .gap(7.0)
        .radius(9.0)
        .fill(c.layer_01)
        .border(1.0, c.border_weak)
        .child(kv("Size on disk", format_bytes(sel.bytes)))
        .child(kv("Share of used space", fmt_pct(sel.bytes as f64 / used as f64)));
    if sel.logical > sel.bytes.saturating_add(1_000_000) {
        metrics = metrics.child(kv("Apparent size", format_bytes(sel.logical)));
    }
    if sel.kind != NodeKind::File {
        metrics = metrics.child(kv("Files inside", pluralize(sel.files as usize, "file", "files")));
    }
    let when = changed_text(sel.mtime, now);
    if !when.is_empty() {
        metrics = metrics.child(kv("Last change", when.trim_start_matches("changed ").to_owned()));
    }
    body = body.child(metrics);

    // What's inside, by deletability. Only worth showing when a folder mixes classes.
    let mixed = Class::ALL.iter().filter(|cl| sel.by_class[cl.idx()] > 0).count() > 1;
    if sel.kind == NodeKind::Dir && mixed {
        let mut mix = col()
            .w_full()
            .padding(12.0)
            .gap(8.0)
            .radius(9.0)
            .fill(c.layer_01)
            .border(1.0, c.border_weak)
            .child(
                text("INSIDE, BY DELETABILITY")
                    .font(tabular_small_medium(k))
                    .color(c.text_faint),
            );
        for class in Class::ALL {
            let b = sel.by_class[class.idx()];
            if b == 0 {
                continue;
            }
            mix = mix.child(
                row()
                    .w_full()
                    .gap(8.0)
                    .center_y()
                    .child(el().size(8.0, 8.0).radius(4.0).fill(class_accent(class, k.dark)))
                    .child(
                        text(class.label())
                            .font(tabular_small(k))
                            .color(c.text_muted)
                            .grow(1.0),
                    )
                    .child(
                        text(format_bytes(b))
                            .font(tabular_small_medium(k))
                            .color(c.text_base),
                    ),
            );
        }
        body = body.child(mix);
    }

    // What to do about it.
    let (heading, advice): (&str, String) = match sel.class {
        Class::Required => (
            "CAN IT BE DELETED?",
            format!("No. {} needs this. It's listed so you can see where space goes, not so you can remove it.", platform::OS_NAME),
        ),
        Class::Yours => (
            "CAN IT BE DELETED?",
            if sel.how.is_empty() {
                "Only you can decide. Cleanse never touches your own files automatically.".into()
            } else {
                format!("Only you can decide. {}", sel.how)
            },
        ),
        Class::Safe | Class::Review => (
            "HOW TO FREE IT",
            if sel.how.is_empty() {
                format!("Reveal it in {} and move it to the {}.", platform::FILE_MANAGER, platform::TRASH_NAME)
            } else {
                sel.how.to_owned()
            },
        ),
    };
    let cli = is_command(sel.how).then(|| command_part(sel.how).to_owned());
    let mut advice_card = col()
        .w_full()
        .padding(12.0)
        .gap(8.0)
        .radius(9.0)
        .fill(c.layer_01)
        .border(1.0, c.border_weak)
        .child(
            row()
                .gap(6.0)
                .center_y()
                .child(icon(&ICON_SHIELD, 13.0).color(class_accent(sel.class, k.dark)))
                .child(text(heading).font(tabular_small_medium(k)).color(c.text_faint)),
        )
        .child(
            text(advice)
                .font(if cli.is_some() && sel.class.can_free() {
                    k.ty.mono.features("'tnum' 1")
                } else {
                    tabular_small(k)
                })
                .color(c.text_base)
                .wrap(),
        );
    if let Some(cmd) = cli.filter(|_| sel.class.can_free()) {
        advice_card = advice_card.child(
            Button::new("Copy command")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Regular)
                .icon(&glyphs::COPY)
                .on_press_tx(move |s: &mut ClearanceApp, tx| {
                    tx.copy(cmd.clone());
                    s.push_toast(
                        "copied-cmd",
                        ToastPayload {
                            title: "Copied command".into(),
                            description: format!("`{cmd}` copied to clipboard"),
                            undo_sweep_id: None,
                        },
                        tx.clock(),
                        tx,
                    );
                })
                .build(k)
                .key("map-insp-copy-cmd"),
        );
    }
    body = body.child(advice_card);

    // Path + actions.
    let reveal = sel.path.clone();
    let copy = path_str.clone();
    let where_path = sel.path.clone();
    let mut path_card = col()
        .w_full()
        .padding(12.0)
        .gap(8.0)
        .radius(9.0)
        .fill(c.layer_01)
        .border(1.0, c.border_weak)
        .child(
            text("LOCATION")
                .font(tabular_small_medium(k))
                .color(c.text_faint),
        )
        .child(
            text(path_str)
                .font(k.ty.mono.features("'tnum' 1"))
                .color(c.text_base)
                .wrap_anywhere(),
        );
    let mut actions = col().w_full().gap(6.0);
    let mut primary = row().w_full().gap(8.0);
    if !is_rollup {
        primary = primary
            .child(
                Button::new(format!("Reveal in {}", platform::FILE_MANAGER))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Regular)
                    .on_press(move |_: &mut ClearanceApp| reveal_in_finder(&reveal))
                    .build(k)
                    .key("map-insp-reveal"),
            )
            .child(
                Button::new("Copy path")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Regular)
                    .icon(&glyphs::COPY)
                    .on_press_tx(move |s: &mut ClearanceApp, tx| {
                        tx.copy(copy.clone());
                        s.push_toast(
                            "copied-path",
                            ToastPayload {
                                title: "Copied path".into(),
                                description: copy.clone(),
                                undo_sweep_id: None,
                            },
                            tx.clock(),
                            tx,
                        );
                    })
                    .build(k)
                    .key("map-insp-copy-path"),
            );
    }
    actions = actions.child(primary);
    if s.map_ui.tab != MapTab::Explorer && !is_rollup {
        actions = actions.child(
            Button::new("Show in Explorer")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Regular)
                .on_press_tx(move |s: &mut ClearanceApp, tx| {
                    s.map_show_in_explorer(&where_path);
                    tx.instant();
                })
                .build(k)
                .key("map-insp-show"),
        );
    }
    path_card = path_card.child(actions);
    body = body.child(path_card);

    // If the regular scanner also found this exact path, offer its undoable cleaning flow.
    if sel.class.can_free()
        && let Some(id) = s.finding_for_path(&sel.path)
    {
        body = body.child(
            col()
                .w_full()
                .padding(12.0)
                .gap(8.0)
                .radius(9.0)
                .fill(c.layer_01)
                .border(1.0, c.border_weak)
                .child(
                    text("Cleanse can clean this safely, with Undo.")
                        .font(tabular_small(k))
                        .color(c.text_muted)
                        .wrap(),
                )
                .child(
                    Button::new("Open in Findings")
                        .variant(ButtonVariant::Contrast)
                        .control_size(ControlSize::Regular)
                        .on_press_tx(move |s: &mut ClearanceApp, tx| {
                            s.route = NavRoute::AllFindings;
                            s.recompute_visible();
                            s.inspected_id = Some(id.clone());
                            s.table.select(id.clone());
                            tx.instant();
                        })
                        .build(k)
                        .key("map-insp-open-finding"),
                ),
        );
    }

    body.child(
        text(sel.class.blurb())
            .font(tabular_small(k))
            .color(c.text_faint)
            .wrap(),
    )
}
