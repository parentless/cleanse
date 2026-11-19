//! Storage Visualization Components (`ui/charts.rs`).
//!
//! Implements the APFS Storage Horizon Bar and all 3 Prototype Hero Analyzer layouts (`prototype` skill):
//! 1. `SpatialBento`: Proportional squarified Spatial Bento Map with `El::press_scale(0.98)`
//!    and `El::matched("cat-card-...")` shared-element zoom transitions into category detail.
//! 2. `HorizonLedger`: Quiet, ultra-dense linear breakdown for keyboard power users.
//! 3. `RunwayRadar`: Analytic vector `rune::Shape` 240° arc gauge + Dormancy Age histogram.

use std::f32::consts::PI;

use rune::{Color, El, LineCap, Shape, Tx, canvas, col, el, icon, oklch, oklcha, row, spacer, text};
use rune_kit::Kit;

use crate::model::{
    Category, DiskOverview, ScanItem, format_bytes, pluralize,
};
use crate::ui::theme::{
    UI_SPRING, card_metric_font, category_accent, category_border, category_icon, category_surface,
    tabular_small, tabular_small_medium, tabular_ui_medium,
};

#[derive(Clone, Copy, Debug)]
pub struct CategoryStat {
    pub category: Category,
    pub total_bytes: u64,
    pub selected_bytes: u64,
    pub item_count: usize,
}

pub fn summarize_categories(items: &[ScanItem], min_dormant_days: u32) -> Vec<CategoryStat> {
    let mut stats: Vec<CategoryStat> = Category::ALL
        .into_iter()
        .map(|cat| {
            let mut total_bytes = 0u64;
            let mut selected_bytes = 0u64;
            let mut item_count = 0usize;
            for it in items.iter().filter(|i| i.category == cat && i.matches_horizon(min_dormant_days)) {
                total_bytes = total_bytes.saturating_add(it.bytes);
                if it.selected && it.is_selectable() {
                    selected_bytes = selected_bytes.saturating_add(it.bytes);
                }
                item_count += 1;
            }
            CategoryStat {
                category: cat,
                total_bytes,
                selected_bytes,
                item_count,
            }
        })
        .collect();
    stats.sort_by(|a, b| b.total_bytes.cmp(&a.total_bytes));
    stats
}

/// Proportional multi-segment APFS Storage Horizon Bar (`System & Active`, `Reclaimable`, `Purgeable`, `Free`).
pub fn storage_horizon_bar(
    disk: &DiskOverview,
    selected_reclaimable: u64,
    reviewable_extra: u64,
    k: &Kit,
) -> El {
    let c = &k.c;
    let total = disk.total_bytes.max(1) as f64;
    let free = disk.free_bytes.min(disk.total_bytes);
    let used = disk.used_bytes();
    let purgeable = disk.purgeable_bytes.min(used);
    let reclaim_sel = selected_reclaimable.min(used.saturating_sub(purgeable));
    let review_rem = reviewable_extra.min(used.saturating_sub(purgeable).saturating_sub(reclaim_sel));
    let core_used = used
        .saturating_sub(purgeable)
        .saturating_sub(reclaim_sel)
        .saturating_sub(review_rem);

    let frac = |b: u64| ((b as f64 / total) as f32).max(0.0);

    let col_core = if k.dark {
        oklch(0.42, 0.03, 255.0)
    } else {
        oklch(0.58, 0.035, 255.0)
    };
    let col_reclaim = if k.dark {
        oklch(0.70, 0.16, 152.0)
    } else {
        oklch(0.55, 0.16, 152.0)
    };
    let col_review = if k.dark {
        oklch(0.74, 0.14, 72.0)
    } else {
        oklch(0.62, 0.14, 72.0)
    };
    let col_purge = if k.dark {
        oklch(0.62, 0.12, 205.0)
    } else {
        oklch(0.68, 0.11, 205.0)
    };
    let col_free = if k.dark {
        oklcha(0.95, 0.0, 0.0, 0.07)
    } else {
        oklcha(0.20, 0.0, 0.0, 0.06)
    };

    let seg = |key: &str, weight: f32, fill: Color| -> El {
        el().key(key)
            .grow(weight.max(0.001))
            .h_full()
            .fill(fill)
            .motion(UI_SPRING)
    };

    let mut bar = row()
        .key("apfs-horizon-track")
        .w_full()
        .h(12.0)
        .radius(6.0)
        .clip_rounded(6.0)
        .gap(1.5)
        .fill(col_free)
        .child(seg("seg-core", frac(core_used).max(0.04), col_core));

    if reclaim_sel > 0 {
        bar = bar.child(seg("seg-reclaim", frac(reclaim_sel).max(0.025), col_reclaim));
    }
    if review_rem > 0 {
        bar = bar.child(seg("seg-review", frac(review_rem).max(0.018), col_review));
    }
    if purgeable > 0 {
        bar = bar.child(seg("seg-purge", frac(purgeable).max(0.02), col_purge));
    }
    bar = bar.child(seg("seg-free", frac(free).max(0.03), col_free));

    let legend_dot = |label: &str, val: u64, swatch: Color| -> El {
        row()
            .gap(6.0)
            .center_y()
            .child(el().size(7.0, 7.0).radius(3.5).fill(swatch))
            .child(
                text(format!("{label} · {}", format_bytes(val)))
                    .font(tabular_small(k))
                    .color(c.text_muted),
            )
    };

    col()
        .key("apfs-horizon-card")
        .w_full()
        .gap(10.0)
        .child(bar)
        .child(
            row()
                .w_full()
                .flex_wrap()
                .gap(16.0)
                .center_y()
                .child(legend_dot("OS & Personal", core_used, col_core))
                .child(legend_dot("Ready to Sweep", reclaim_sel, col_reclaim))
                .child(legend_dot("Opt-In / Inspect", review_rem, col_review))
                .child(legend_dot(
                    &if disk.local_snapshots > 0 { format!("Purgeable ({} snapshots)", disk.local_snapshots) } else { "Purgeable".to_owned() },
                    purgeable,
                    col_purge,
                ))
                .child(spacer())
                .child(
                    text(format!(
                        "{} free of {}",
                        format_bytes(free),
                        format_bytes(disk.total_bytes)
                    ))
                    .font(tabular_small_medium(k))
                    .color(c.text_base),
                ),
        )
}

/// Prototype 1 (`SpatialBento`): Proportional 2-row squarified Spatial Bento Map.
/// Every tile carries `.press_scale(0.98)` and `.matched("cat-card-<id>")` so clicking zooms
/// into the Category Detail view with a shared-element flight!
pub fn spatial_bento_map<S: 'static>(
    stats: &[CategoryStat],
    k: &Kit,
    on_pick: impl Fn(&mut S, Category, &mut Tx) + Clone + 'static,
) -> El {
    let non_zero: Vec<CategoryStat> = stats
        .iter()
        .copied()
        .filter(|s| s.total_bytes > 0)
        .collect();

    if non_zero.is_empty() {
        return empty_hero_placeholder(k);
    }

    let max_bytes = non_zero
        .iter()
        .map(|s| s.total_bytes)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    // Normalized visual weights (dampened sqrt curve so a 1 TB outlier never crushes a 500 MB tile to 2px!)
    let weight_of = |b: u64| -> f32 {
        let ratio = (b as f64 / max_bytes).clamp(0.02, 1.0);
        (ratio.powf(0.42) as f32).clamp(0.28, 1.0)
    };

    let build_tile = |stat: CategoryStat, tall: bool| -> El {
        let cat = stat.category;
        let accent = category_accent(cat, k.dark);
        let bg = category_surface(cat, k.dark, false);
        let border = category_border(cat, k.dark, false);
        let matched_id = format!("cat-card-{}", cat.id());
        let pick_cb = on_pick.clone();

        let status_line = if cat == Category::Heavyweights {
            "Inspect only · locked".to_string()
        } else if stat.selected_bytes > 0 {
            format!("{} staged", format_bytes(stat.selected_bytes))
        } else {
            pluralize(stat.item_count, "item", "items")
        };

        col()
            .key(format!("bento-{}", cat.id()))
            .matched(matched_id)
            .matched_crop()
            .grow(weight_of(stat.total_bytes))
            .min_w(132.0)
            .h(if tall { 118.0 } else { 96.0 })
            .padding(14.0)
            .justify_between()
            .radius(12.0)
            .fill(bg)
            .border(1.0, border)
            .hover(k.c.hover)
            .press_scale(0.98)
            .motion(UI_SPRING)
            .label(format!("{}: {}", cat.title(), format_bytes(stat.total_bytes)))
            .on_press_tx(move |s: &mut S, tx: &mut Tx| pick_cb(s, cat, tx))
            .child(
                row()
                    .w_full()
                    .center_y()
                    .gap(7.0)
                    .child(icon(category_icon(cat), 15.0).color(accent))
                    .child(
                        text(cat.short_title())
                            .font(tabular_ui_medium(k))
                            .color(k.c.text_base)
                            .truncate(),
                    )
                    .child(spacer())
                    .child(
                        text(pluralize(stat.item_count, "item", "items"))
                            .font(tabular_small(k))
                            .color(k.c.text_faint),
                    ),
            )
            .child(
                col()
                    .w_full()
                    .gap(2.0)
                    .child(
                        text(format_bytes(stat.total_bytes))
                            .font(card_metric_font(k))
                            .color(k.c.text_base)
                            .truncate(),
                    )
                    .child(
                        text(status_line)
                            .font(tabular_small(k))
                            .color(if stat.selected_bytes > 0 {
                                accent
                            } else {
                                k.c.text_muted
                            })
                            .truncate(),
                    ),
            )
    };

    let split_at = non_zero.len().min(3);
    let top_slice = &non_zero[..split_at];
    let bottom_slice = &non_zero[split_at..];

    let mut top_row = row().key("bento-row-top").w_full().gap(10.0);
    for st in top_slice {
        top_row = top_row.child(build_tile(*st, true));
    }

    let mut container = col().key("spatial-bento-stage").w_full().gap(10.0).child(top_row);
    if !bottom_slice.is_empty() {
        let mut bot_row = row().key("bento-row-bot").w_full().gap(10.0);
        for st in bottom_slice {
            bot_row = bot_row.child(build_tile(*st, false));
        }
        container = container.child(bot_row);
    }
    container
}

/// Prototype 2 (`HorizonLedger`): Quiet, ultra-dense linear ledger for keyboard-centric daily users.
pub fn horizon_ledger_view<S: 'static>(
    stats: &[CategoryStat],
    k: &Kit,
    on_pick: impl Fn(&mut S, Category, &mut Tx) + Clone + 'static,
) -> El {
    let total_all: u64 = stats.iter().map(|s| s.total_bytes).sum::<u64>().max(1);
    if stats.iter().all(|s| s.total_bytes == 0) {
        return empty_hero_placeholder(k);
    }

    let mut list = col()
        .key("horizon-ledger-stage")
        .w_full()
        .radius(12.0)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_weak)
        .padding(8.0)
        .gap(2.0);

    for st in stats.iter().filter(|s| s.total_bytes > 0) {
        let cat = st.category;
        let share = (st.total_bytes as f64 / total_all as f64).clamp(0.01, 1.0) as f32;
        let accent = category_accent(cat, k.dark);
        let pick_cb = on_pick.clone();

        let row_el = row()
            .key(format!("ledger-{}", cat.id()))
            .matched(format!("cat-card-{}", cat.id()))
            .matched_crop()
            .w_full()
            .h(34.0)
            .px(10.0)
            .gap(12.0)
            .radius(7.0)
            .center_y()
            .hover(k.c.hover)
            .press_scale(0.99)
            .on_press_tx(move |s: &mut S, tx: &mut Tx| pick_cb(s, cat, tx))
            .child(icon(category_icon(cat), 14.0).color(accent))
            .child(
                text(cat.title())
                    .w(188.0)
                    .font(tabular_ui_medium(k))
                    .color(k.c.text_base)
                    .truncate(),
            )
            .child(
                row()
                    .grow(1.0)
                    .h(7.0)
                    .radius(3.5)
                    .clip_rounded(3.5)
                    .fill(if k.dark {
                        oklcha(1.0, 0.0, 0.0, 0.06)
                    } else {
                        oklcha(0.0, 0.0, 0.0, 0.06)
                    })
                    .child(
                        el().w_frac(share)
                            .h_full()
                            .radius(3.5)
                            .fill(accent)
                            .motion(UI_SPRING),
                    ),
            )
            .child(
                text(format!("{:>3.0}%", share * 100.0))
                    .w(44.0)
                    .font(tabular_small(k))
                    .color(k.c.text_faint),
            )
            .child(
                text(pluralize(st.item_count, "item", "items"))
                    .w(74.0)
                    .font(tabular_small(k))
                    .color(k.c.text_muted),
            )
            .child(
                text(format_bytes(st.total_bytes))
                    .w(86.0)
                    .font(tabular_ui_medium(k))
                    .color(k.c.text_base),
            );
        list = list.child(row_el);
    }

    list
}

/// Prototype 3 (`RunwayRadar`): Analytic `rune::Shape` 240° storage pressure arc gauge + Dormancy Age histogram.
pub fn runway_radar_view(
    disk: &DiskOverview,
    items: &[ScanItem],
    selected_reclaimable: u64,
    k: &Kit,
) -> El {
    let c = &k.c;
    let used_frac = disk.used_fraction();
    let after_sweep_used = disk.used_bytes().saturating_sub(selected_reclaimable);
    let after_frac = if disk.total_bytes > 0 {
        (after_sweep_used as f64 / disk.total_bytes as f64).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };

    let arc_pts = |cx: f32, cy: f32, r: f32, t0: f32, t1: f32, steps: usize| -> Vec<[f32; 2]> {
        let a_start = PI * 0.8333; // 150 deg
        let span = PI * 1.3333; // 240 deg sweep
        (0..=steps)
            .map(|i| {
                let u = t0 + (t1 - t0) * (i as f32 / steps.max(1) as f32);
                let ang = a_start + span * u;
                [cx + ang.cos() * r, cy + ang.sin() * r]
            })
            .collect()
    };

    let track_col = if k.dark {
        oklcha(0.95, 0.0, 0.0, 0.09)
    } else {
        oklcha(0.15, 0.0, 0.0, 0.09)
    };
    let used_col = if used_frac > 0.88 {
        oklch(0.68, 0.17, 32.0)
    } else {
        oklch(0.66, 0.15, 252.0)
    };
    let reclaim_col = oklch(0.72, 0.15, 152.0);

    let mut gauge = Shape::new()
        .view_box(180.0, 142.0)
        .stroke_with(
            arc_pts(90.0, 84.0, 58.0, 0.0, 1.0, 48),
            false,
            11.0,
            LineCap::Round,
            track_col,
        );
    if after_frac > 0.005 {
        gauge = gauge.stroke_with(
            arc_pts(90.0, 84.0, 58.0, 0.0, after_frac, 40),
            false,
            11.0,
            LineCap::Round,
            used_col,
        );
    }
    if used_frac > after_frac + 0.004 {
        gauge = gauge.stroke_with(
            arc_pts(90.0, 84.0, 58.0, after_frac, used_frac, 24),
            false,
            11.0,
            LineCap::Round,
            reclaim_col,
        );
    }

    let dial = el()
        .key("radar-dial")
        .size(190.0, 148.0)
        .child(canvas(gauge).size(190.0, 148.0))
        .child(
            col()
                .absolute(0.0, 52.0)
                .w(190.0)
                .items_center()
                .gap(2.0)
                .child(
                    text(format!("{:.0}%", after_frac * 100.0))
                        .font(card_metric_font(k))
                        .color(c.text_base),
                )
                .child(
                    text("Post-Sweep Used")
                        .font(tabular_small(k))
                        .color(c.text_muted),
                ),
        );

    // Dormancy Age Buckets: <7d, 7-30d, 30-90d, 90d+
    let buckets: [(&str, u32, u32, f32); 4] = [
        ("Active (< 7d)", 0, 7, 252.0),
        ("Recent (7–30d)", 7, 30, 195.0),
        ("Dormant (30–90d)", 30, 90, 152.0),
        ("Cold Hoard (90d+)", 90, u32::MAX, 72.0),
    ];
    let mut bucket_vals = [0u64; 4];
    for it in items {
        for (idx, (_, lo, hi, _)) in buckets.iter().enumerate() {
            if it.age_days >= *lo && it.age_days < *hi {
                bucket_vals[idx] = bucket_vals[idx].saturating_add(it.bytes);
                break;
            }
        }
    }
    let max_b = (*bucket_vals.iter().max().unwrap_or(&1)).max(1) as f64;

    let mut timeline = col().grow(1.0).gap(10.0).child(
        text("Storage Dormancy Distribution")
            .font(tabular_ui_medium(k))
            .color(c.text_base),
    );
    for (idx, (label, _, _, hue)) in buckets.iter().enumerate() {
        let bytes = bucket_vals[idx];
        let pct = ((bytes as f64 / max_b) as f32).clamp(0.02, 1.0);
        let swatch = if k.dark {
            oklch(0.72, 0.14, *hue)
        } else {
            oklch(0.54, 0.15, *hue)
        };
        timeline = timeline.child(
            row()
                .w_full()
                .center_y()
                .gap(10.0)
                .child(
                    text(*label)
                        .w(128.0)
                        .font(tabular_small_medium(k))
                        .color(c.text_muted),
                )
                .child(
                    row()
                        .grow(1.0)
                        .h(8.0)
                        .radius(4.0)
                        .fill(track_col)
                        .child(
                            el().w_frac(pct)
                                .h_full()
                                .radius(4.0)
                                .fill(swatch)
                                .motion(UI_SPRING),
                        ),
                )
                .child(
                    text(format_bytes(bytes))
                        .w(84.0)
                        .font(tabular_small_medium(k))
                        .color(c.text_base),
                ),
        );
    }

    row()
        .key("runway-radar-stage")
        .w_full()
        .padding(16.0)
        .gap(24.0)
        .center_y()
        .radius(12.0)
        .fill(c.layer_01)
        .border(1.0, c.border_weak)
        .child(dial)
        .child(timeline)
}

fn empty_hero_placeholder(k: &Kit) -> El {
    col()
        .key("empty-hero")
        .w_full()
        .padding(24.0)
        .center()
        .gap(6.0)
        .radius(12.0)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_weak)
        .child(
            text("Spotless — Zero Reclaimable Clutter Found")
                .font(tabular_ui_medium(k))
                .color(k.c.text_base),
        )
        .child(
            text("No regenerable caches, dormant build artifacts, or stale installers match your current filter.")
                .font(tabular_small(k))
                .color(k.c.text_muted),
        )
}
