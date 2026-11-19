mod cleaner;
mod diskmap;
mod fixtures;
mod model;
mod platform;
mod safety;
mod scanner;
mod ui;

use rune::{AppSpec, Chrome, Tx};

use crate::model::{Category, DatasetMode, HeroVariant};
use crate::ui::diskmap_view::{MapTab, Sel};
use crate::ui::{ClearanceApp, NavRoute, on_key, poll, view};

const SCENES: &[(&str, fn(&mut ClearanceApp))] = &[
    ("default", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
    }),
    ("light", |s| {
        s.dark_override = Some(false);
        s.switch_dataset(DatasetMode::Demo);
    }),
    ("category-zoom", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
        s.route = NavRoute::CategoryFocus(Category::DevArtifacts);
        s.recompute_visible();
    }),
    ("vault", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
        let mut tx = Tx::default();
        s.trigger_sweep(&mut tx);
        s.route = NavRoute::QuarantineVault;
    }),
    ("safety", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
        s.route = NavRoute::SafetyConstitution;
    }),
    ("cmd-k", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
        s.palette_open = true;
    }),
    ("worst-case", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::WorstCase);
        s.dev_harness_open = true;
    }),
    ("diskmap", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
        s.route = NavRoute::DiskMap;
    }),
    ("diskmap-deep", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
        s.route = NavRoute::DiskMap;
        s.map_ui.nav = vec!["Users".into(), "demo".into(), "Desktop".into()];
        s.map_ui.selected = s.map.dir_at(&s.map_ui.nav).and_then(|d| d.find_child("quebec")).map(Sel::from_node);
    }),
    ("diskmap-delete", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
        s.route = NavRoute::DiskMap;
        s.map_ui.tab = MapTab::CanDelete;
        s.map_ui.selected = s.map.candidates.first().map(Sel::from_candidate);
    }),
    ("diskmap-files", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
        s.route = NavRoute::DiskMap;
        s.map_ui.tab = MapTab::BiggestFiles;
    }),
    ("diskmap-types", |s| {
        s.dark_override = Some(false);
        s.switch_dataset(DatasetMode::Demo);
        s.route = NavRoute::DiskMap;
        s.map_ui.tab = MapTab::ByType;
    }),
    ("empty", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Empty);
    }),
    ("single-item", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::SingleItem);
    }),
    ("proto-ledger", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
        s.hero_variant = HeroVariant::HorizonLedger;
    }),
    ("proto-radar", |s| {
        s.dark_override = Some(true);
        s.switch_dataset(DatasetMode::Demo);
        s.hero_variant = HeroVariant::RunwayRadar;
    }),
];

fn main() {
    rune::set_app_id("dev.cleanse.app");
    let chrome = Chrome {
        full_size_content: true,
        traffic_lights: Some((16.0, 25.0)),
        drag_regions: true,
    };
    let spec = AppSpec {
        min_size: Some((860.0, 620.0)),
        scenes: SCENES,
        families: ("Inter", "ui-monospace"),
        setup: Some(ClearanceApp::setup),
        poll: Some(poll),
        on_key: Some(on_key),
        ..AppSpec::new("Cleanse", (1280.0, 820.0), ClearanceApp::init, view)
    };
    rune::App::new(spec).chrome(chrome).run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CleanStrategy, pluralize};
    use rune::{Env, Key, KeyEvent, Mods, Runtime};

    fn headless_runtime() -> Runtime<ClearanceApp> {
        let mut state = ClearanceApp::init();
        state.switch_dataset(DatasetMode::Demo);
        let mut rt = Runtime::new(state, view, (1280.0, 820.0), Env::new(true, 2.0));
        rt.poll = Some(poll);
        rt.on_key = Some(on_key);
        rt
    }

    #[test]
    fn pluralize_never_renders_1_items() {
        assert_eq!(pluralize(0, "item", "items"), "0 items");
        assert_eq!(pluralize(1, "item", "items"), "1 item");
        assert_eq!(pluralize(2, "item", "items"), "2 items");
        assert_eq!(pluralize(1_420, "file", "files"), "1,420 files");
    }

    #[test]
    fn command_palette_opens_and_closes_instantly_at_zero_ms() {
        let mut rt = headless_runtime();
        rt.frame(0.0);
        assert!(rt.presented("cmdk-dialog").is_none());

        // Press ⌘K — must mount immediately on the very next frame with full opacity (0ms)
        rt.key(KeyEvent {
            key: Key::Command('k'),
            mods: Mods {
                cmd: true,
                logo: true,
                ..Default::default()
            },
        });
        rt.frame(1.0 / 120.0);
        let (rect, opacity) = rt.presented("cmdk-dialog").expect("cmdk dialog mounted");
        assert!(rect[2] > 400.0 && rect[3] > 100.0);
        assert!(
            (opacity - 1.0).abs() < 0.01,
            "Expected 0ms instant full opacity, got {opacity}"
        );

        // Press Escape — must unmount immediately with zero lingering exit ghost
        rt.key(KeyEvent {
            key: Key::Escape,
            mods: Mods::default(),
        });
        rt.frame(1.0 / 120.0);
        assert!(rt.presented("cmdk-dialog").is_none());
    }

    #[test]
    fn dormancy_slider_tracks_drag_and_hold_to_sweep_cancels_on_early_release() {
        let mut rt = headless_runtime();
        rt.frame(0.0);

        // 1. Drag Dormancy Horizon Slider across its rail and past the right edge (rubber-band)
        let slider = rt
            .presented("dormancy-slider-hit")
            .expect("slider visible")
            .0;
        let sy = slider[1] + slider[3] * 0.5;
        rt.pointer_down(slider[0] + 12.0, sy);
        rt.pointer_move(slider[0] + slider[2] + 40.0, sy);
        rt.frame(1.0 / 120.0);
        assert_eq!(rt.state.horizon.days, 90);
        assert!(
            rt.state.horizon.rubber_px > 0.0,
            "Expected positive rubber-band overscroll past 90d"
        );
        rt.pointer_up(slider[0] + slider[2] + 40.0, sy);
        rt.frame(1.0 / 120.0);
        assert_eq!(rt.state.horizon.rubber_px, 0.0);

        // Reset horizon to 0d so items are eligible
        rt.pointer_down(slider[0] + 2.0, sy);
        rt.pointer_up(slider[0] + 2.0, sy);
        rt.frame(1.0 / 120.0);
        assert_eq!(rt.state.horizon.days, 0);

        // 2. Press Hold-to-Sweep for only 0.30s (< 1.05s) and release early -> MUST cancel cleanly!
        let count_before = rt.state.items.len();
        let sweep_btn = rt.presented("hold-sweep-btn").expect("sweep btn visible").0;
        let bx = sweep_btn[0] + sweep_btn[2] * 0.5;
        let by = sweep_btn[1] + sweep_btn[3] * 0.5;
        rt.pointer_down(bx, by);
        rt.frame(1.0 / 120.0);
        rt.frame(0.30);
        assert!(rt.state.hold_sweep.holding);
        assert!(rt.state.hold_sweep.progress > 0.15 && rt.state.hold_sweep.progress < 0.90);
        rt.pointer_up(bx, by);
        rt.frame(0.05);
        assert!(!rt.state.hold_sweep.holding);
        assert_eq!(
            rt.state.items.len(),
            count_before,
            "Early release must never trigger sweep"
        );

        // 3. Hold past 1.05s -> MUST complete Quarantine Vault sweep!
        rt.pointer_down(bx, by);
        rt.frame(1.0 / 120.0);
        rt.frame(1.15);
        rt.pointer_up(bx, by);
        rt.frame(0.05);
        assert!(rt.state.items.len() < count_before);
        assert_eq!(rt.state.vault_count(), 1);
    }

    #[test]
    fn shared_element_category_zoom_and_vault_roundtrip_work() {
        let mut rt = headless_runtime();
        rt.frame(0.0);

        let initial_count = rt.state.items.len();
        let tile_rect = rt
            .presented("bento-dev-artifacts")
            .expect("dev artifacts tile rendered")
            .0;
        assert!(tile_rect[2] > 100.0);

        // Sweep into Quarantine Vault and verify 1-click Undo restores exact item count!
        rt.update(|s, tx| {
            s.strategy = CleanStrategy::QuarantineVault;
            s.trigger_sweep(tx);
        });
        rt.frame(1.0 / 60.0);
        assert!(rt.state.items.len() < initial_count);
        assert_eq!(rt.state.vault_count(), 1);

        let sid = rt.state.synthetic_vault[0].manifest.sweep_id.clone();
        rt.update(|s, tx| {
            s.undo_sweep(&sid, tx);
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.items.len(), initial_count);
        assert_eq!(rt.state.vault_count(), 0);
    }

    #[test]
    fn worst_case_and_5k_datasets_layout_cleanly_at_narrow_width() {
        let mut state = ClearanceApp::init();
        state.switch_dataset(DatasetMode::WorstCase);
        let mut rt = Runtime::new(state, view, (860.0, 620.0), Env::new(true, 2.0));
        rt.frame(0.0);
        let dock = rt.presented("sweep-action-dock").expect("dock visible").0;
        // Dock must stay strictly inside the 860x620 window bounds without horizontal overflow!
        assert!(
            dock[0] >= 0.0 && dock[0] + dock[2] <= 860.5,
            "dock overflowed: {dock:?}"
        );

        rt.update(|s, _| s.switch_dataset(DatasetMode::Massive5k));
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.items.len(), 5_000);
    }

    #[test]
    fn live_apfs_disk_overview_reads_real_volume_metrics() {
        let overview = crate::scanner::query_disk_overview();
        assert!(overview.total_bytes > 1_000_000_000);
        assert!(overview.free_bytes <= overview.total_bytes);
    }

    fn click(rt: &mut Runtime<ClearanceApp>, key: &str) {
        let r = rt
            .presented(key)
            .unwrap_or_else(|| panic!("`{key}` not on screen"))
            .0;
        let (x, y) = (r[0] + r[2] * 0.5, r[1] + r[3] * 0.5);
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(1.0 / 120.0);
    }

    #[test]
    fn disk_map_drills_into_folders_and_escape_goes_back_up() {
        let mut rt = headless_runtime();
        rt.update(|s, _| s.route = NavRoute::DiskMap);
        rt.frame(0.0);
        assert!(rt.state.map_ui.nav.is_empty());

        click(&mut rt, "map-row-/Users-Users");
        assert_eq!(rt.state.map_ui.nav, ["Users"]);
        click(&mut rt, "map-row-/Users/demo-demo");
        assert_eq!(rt.state.map_ui.nav, ["Users", "demo"]);

        let esc = KeyEvent {
            key: Key::Escape,
            mods: Mods::default(),
        };
        rt.key(esc.clone());
        rt.frame(1.0 / 120.0);
        assert_eq!(rt.state.map_ui.nav, ["Users"]);
        rt.key(esc);
        rt.frame(1.0 / 120.0);
        assert!(rt.state.map_ui.nav.is_empty());
    }

    #[test]
    fn disk_map_class_filter_hides_folders_that_hold_none_of_it() {
        let mut rt = headless_runtime();
        rt.update(|s, _| s.route = NavRoute::DiskMap);
        rt.frame(0.0);
        assert!(rt.presented("map-row-/System-System").is_some());

        click(&mut rt, "map-chip-Safe");
        assert_eq!(rt.state.map_ui.filter, Some(crate::diskmap::Class::Safe));
        assert!(
            rt.presented("map-row-/System-System").is_none(),
            "macOS system files hold no safe-to-delete bytes"
        );
        assert!(rt.presented("map-row-/Users-Users").is_some());

        click(&mut rt, "map-chip-Safe");
        assert_eq!(rt.state.map_ui.filter, None);
    }

    #[test]
    fn disk_map_can_delete_tab_lists_only_deletable_things() {
        let mut rt = headless_runtime();
        rt.update(|s, _| {
            s.route = NavRoute::DiskMap;
            s.map_ui.tab = crate::ui::diskmap_view::MapTab::CanDelete;
        });
        rt.frame(0.0);
        assert!(!rt.state.map.candidates.is_empty());
        assert!(
            rt.state
                .map
                .candidates
                .iter()
                .all(|c| c.class.can_free() && !c.path.starts_with("/System"))
        );
        let first = rt.state.map.candidates[0].path.display().to_string().replace('\\', "/");
        click(&mut rt, &format!("cand-{first}"));
        assert_eq!(
            rt.state.map_ui.selected.as_ref().map(|s| s.path.display().to_string().replace('\\', "/")),
            Some(first)
        );
    }

    #[test]
    fn disk_map_stays_inside_a_narrow_window_on_every_tab() {
        use crate::ui::diskmap_view::MapTab;
        for tab in [MapTab::Explorer, MapTab::CanDelete, MapTab::BiggestFiles, MapTab::ByType] {
            let mut state = ClearanceApp::init();
            state.switch_dataset(DatasetMode::Demo);
            state.route = NavRoute::DiskMap;
            state.map_ui.tab = tab;
            let mut rt = Runtime::new(state, view, (860.0, 620.0), Env::new(true, 2.0));
            rt.frame(0.0);
            let hero = rt.presented("map-hero").expect("hero visible").0;
            assert!(
                hero[0] >= 0.0 && hero[0] + hero[2] <= 860.5,
                "{tab:?}: hero overflowed {hero:?}"
            );
        }
    }

    #[test]
    fn disk_map_empty_and_scanning_states_render() {
        let mut state = ClearanceApp::init();
        state.switch_dataset(DatasetMode::Demo);
        state.map = crate::diskmap::MapState::empty();
        state.route = NavRoute::DiskMap;
        let mut rt = Runtime::new(state, view, (1280.0, 820.0), Env::new(true, 2.0));
        rt.frame(0.0);
        assert!(rt.presented("map-hero").is_some());
        rt.update(|s, _| s.map.scanning = true);
        rt.frame(1.0 / 60.0);
        assert!(rt.presented("map-hero").is_some());
    }
}
