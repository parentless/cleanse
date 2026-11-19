//! Custom Gesture & Motion Controls (`ui/controls.rs`).
//!
//! Implements:
//! 1. **Dormancy Horizon Slider (`0d..90d`)** (`gestures` + `apple-design`):
//!    - 1:1 `Phase::Move` pointer tracking with zero spring lag
//!    - Grab-offset preservation on thumb press (`Drag::data`)
//!    - Exponential rubber-band edge resistance past `0d` and `90d`, springing back on release
//! 2. **Asymmetric Hold-to-Sweep Button** (`emil-design-eng` + `animate`):
//!    - Deliberate `1.05s` linear fill progress while held to prevent accidental sweeps
//!    - Snappy `180ms` release spring if the user lets go early
//!    - Instant click in `DryRun` mode or via keyboard / Reduced Motion accessibility activation
//! 3. **Safety Tier Badges & Active-Process Lock Chips**

use std::time::Duration;

use rune::{Cursor, Drag, El, Phase, Tx, col, el, icon, oklch, oklcha, row, text};
use rune_kit::{Kit, glyphs};

use crate::model::{CleanStrategy, SafetyTier};
use crate::ui::theme::{
    CONTROL_SPRING, ICON_SHIELD, RELEASE_SPRING, safety_colors, tabular_small,
    tabular_small_medium, tabular_ui_medium,
};

/// State owned by `ClearanceApp` for the 1:1 Dormancy Horizon Slider (`0..=90` days).
#[derive(Clone, Copy, Debug, Default)]
pub struct HorizonSliderState {
    pub days: u32,
    pub dragging: bool,
    pub start_days: u32,
    /// Visual rubber-band overscroll in points when dragged past `[0, width]`.
    pub rubber_px: f32,
}

/// Exponential rubber-band resistance (`gestures` skill: `c ≈ 0.4`, capped cleanly).
pub fn rubberband(overshoot: f32, limit: f32) -> f32 {
    if overshoot.abs() < 0.001 || limit <= 0.0 {
        return 0.0;
    }
    let sign = overshoot.signum();
    let d = overshoot.abs();
    sign * limit * (1.0 - (-d / (limit * 2.4)).exp())
}

pub fn dormancy_slider<S: 'static>(
    state: HorizonSliderState,
    k: &Kit,
    get_mut: fn(&mut S) -> &mut HorizonSliderState,
    on_change: fn(&mut S),
) -> El {
    const TRACK_W: f32 = 176.0;
    const THUMB_SZ: f32 = 16.0;
    const MAX_DAYS: f32 = 90.0;

    let norm = (state.days as f32 / MAX_DAYS).clamp(0.0, 1.0);
    let usable_w = TRACK_W - THUMB_SZ;
    let thumb_x = norm * usable_w + state.rubber_px;
    let fill_w = (thumb_x + THUMB_SZ * 0.5).clamp(4.0, TRACK_W);

    let track_bg = if k.dark {
        oklcha(1.0, 0.0, 0.0, 0.09)
    } else {
        oklcha(0.0, 0.0, 0.0, 0.09)
    };
    let accent = if k.dark {
        oklch(0.68, 0.15, 252.0)
    } else {
        oklch(0.54, 0.16, 252.0)
    };

    let day_readout = if state.days == 0 {
        "All ages".to_string()
    } else {
        format!("≥ {}d old", state.days)
    };

    let track_interactive = el()
        .key("dormancy-slider-hit")
        .role("slider")
        .label("Dormancy Horizon Filter")
        .description(day_readout.clone())
        .size(TRACK_W, 24.0)
        .cursor(if state.dragging {
            Cursor::Grabbing
        } else {
            Cursor::Pointer
        })
        .focusable(true)
        .focus_ring(2.0, 2.0, k.c.border_focus)
        .touch_drags_at_once()
        .on_drag(move |s: &mut S, d: &mut Drag, tx: &mut Tx| {
            let w = (d.w - THUMB_SZ).max(1.0);
            match d.phase {
                Phase::Down => {
                    let st = get_mut(s);
                    st.dragging = true;
                    st.start_days = st.days;
                    let cur_center = (st.days as f32 / MAX_DAYS) * w + THUMB_SZ * 0.5;
                    // Preserve grab offset if pressing directly on thumb; otherwise jump to press x
                    if (d.x - cur_center).abs() <= THUMB_SZ * 0.85 {
                        d.data = d.x - cur_center;
                    } else {
                        d.data = 0.0;
                        let raw = d.x - THUMB_SZ * 0.5;
                        let clamped = raw.clamp(0.0, w);
                        st.days = ((clamped / w) * MAX_DAYS).round() as u32;
                        st.rubber_px = 0.0;
                        on_change(s);
                    }
                    tx.instant();
                }
                Phase::Move => {
                    let raw = (d.x - d.data) - THUMB_SZ * 0.5;
                    let clamped = raw.clamp(0.0, w);
                    let over = if raw < 0.0 {
                        raw
                    } else if raw > w {
                        raw - w
                    } else {
                        0.0
                    };
                    let new_days = ((clamped / w) * MAX_DAYS).round() as u32;
                    let st = get_mut(s);
                    let changed = st.days != new_days;
                    st.days = new_days;
                    st.rubber_px = rubberband(over, 10.0);
                    if changed {
                        on_change(s);
                    }
                    // 1:1 pointer tracking: zero spring lag during drag!
                    tx.instant();
                }
                Phase::Up => {
                    let cancelled = d.cancelled();
                    let st = get_mut(s);
                    st.dragging = false;
                    st.rubber_px = 0.0;
                    if cancelled {
                        st.days = st.start_days;
                        on_change(s);
                    }
                    tx.animated();
                    tx.motion(RELEASE_SPRING);
                }
            }
        })
        // Visual track rail
        .child(
            el().key("slider-rail")
                .absolute(0.0, 9.0)
                .size(TRACK_W, 6.0)
                .radius(3.0)
                .fill(track_bg)
                .clip_rounded(3.0)
                .child(
                    el().key("slider-fill")
                        .size(fill_w, 6.0)
                        .radius(3.0)
                        .fill(accent),
                ),
        )
        // Physical thumb knob
        .child(
            el().key("slider-thumb")
                .absolute(thumb_x, 4.0)
                .size(THUMB_SZ, THUMB_SZ)
                .radius(THUMB_SZ * 0.5)
                .fill(rune::Color::WHITE)
                .border(1.0, oklcha(0.0, 0.0, 0.0, 0.18))
                .shadow(1.5, 4.0, oklcha(0.0, 0.0, 0.0, 0.28))
                .scale(if state.dragging { 1.08 } else { 1.0 })
                .motion(RELEASE_SPRING),
        );

    row()
        .key("dormancy-control")
        .center_y()
        .gap(10.0)
        .child(
            col()
                .gap(1.0)
                .child(
                    text("Build Dormancy")
                        .font(tabular_small(k))
                        .color(k.c.text_faint),
                )
                .child(
                    text(day_readout)
                        .w(68.0)
                        .font(tabular_small_medium(k))
                        .color(k.c.text_base),
                ),
        )
        .child(track_interactive)
}

/// State for the Asymmetric Hold-to-Sweep confirmation button (`1.05s` linear hold, `180ms` release).
#[derive(Clone, Copy, Debug, Default)]
pub struct HoldSweepState {
    pub holding: bool,
    pub started_at: f64,
    pub progress: f32,
    pub fired_this_press: bool,
}

impl HoldSweepState {
    pub const HOLD_DURATION_SECS: f64 = 1.05;
}

#[allow(clippy::too_many_arguments)]
pub fn hold_to_sweep_button<S: 'static>(
    hold: HoldSweepState,
    strategy: CleanStrategy,
    enabled: bool,
    reduced_motion: bool,
    k: &Kit,
    get_mut: fn(&mut S) -> &mut HoldSweepState,
    on_trigger: fn(&mut S, &mut Tx),
) -> El {
    let is_instant_mode = strategy == CleanStrategy::DryRun || reduced_motion;
    let label_str = if !enabled {
        "Select Items to Sweep"
    } else if hold.holding && !is_instant_mode {
        "Keep Holding to Confirm…"
    } else if reduced_motion && strategy != CleanStrategy::DryRun {
        match strategy {
            CleanStrategy::QuarantineVault => "Stage Safe Sweep",
            CleanStrategy::MacTrash => "Move Selected to Trash",
            CleanStrategy::DryRun => "Run Safety Simulation",
        }
    } else {
        strategy.action_verb()
    };

    let base_fill = if !enabled {
        k.c.surface_disabled
    } else if strategy == CleanStrategy::DryRun {
        if k.dark {
            oklch(0.32, 0.04, 252.0)
        } else {
            oklch(0.90, 0.03, 252.0)
        }
    } else if k.dark {
        oklch(0.56, 0.155, 152.0)
    } else {
        oklch(0.49, 0.155, 152.0)
    };

    let progress_fill = if k.dark {
        oklch(0.68, 0.165, 152.0)
    } else {
        oklch(0.40, 0.16, 152.0)
    };

    let fg = if !enabled {
        k.c.text_faint
    } else if strategy == CleanStrategy::DryRun && !k.dark {
        k.c.text_base
    } else {
        rune::Color::WHITE
    };

    let mut btn = row()
        .key("hold-sweep-btn")
        .role("button")
        .label(label_str)
        .h(36.0)
        .min_w(212.0)
        .px(16.0)
        .radius(9.0)
        .clip_rounded(9.0)
        .center()
        .gap(8.0)
        .fill(base_fill)
        .focusable(enabled)
        .disabled(!enabled)
        .focus_ring(2.0, 2.0, k.c.border_focus);

    if enabled {
        btn = btn
            .cursor(Cursor::Pointer)
            .press_scale(0.98)
            .hover(oklcha(1.0, 1.0, 1.0, 0.08));
    }

    // Asymmetric progress fill layer underneath label
    if enabled && !is_instant_mode && hold.progress > 0.002 {
        btn = btn.child(
            el().key("hold-sweep-fill")
                .absolute(0.0, 0.0)
                .w_frac(hold.progress.clamp(0.0, 1.0))
                .h_full()
                .fill(progress_fill)
                .motion(if hold.holding {
                    rune::Spring::NONE
                } else {
                    CONTROL_SPRING
                }),
        );
    }

    btn = btn
        .child(icon(&ICON_SHIELD, 15.0).color(fg).a11y_hidden())
        .child(
            text(label_str)
                .font(tabular_ui_medium(k))
                .color(fg)
                .a11y_hidden(),
        );

    if !enabled {
        return btn;
    }

    if is_instant_mode {
        btn.on_press_tx(move |s: &mut S, tx: &mut Tx| {
            on_trigger(s, tx);
        })
    } else {
        btn.touch_drags_at_once()
            // Keyboard / assistive activation triggers directly without requiring a 1.05s pointer hold
            .on_press_tx(move |s: &mut S, tx: &mut Tx| {
                if tx.by_keys() || tx.assistive_technology() {
                    on_trigger(s, tx);
                }
            })
            .on_drag(move |s: &mut S, d: &mut Drag, tx: &mut Tx| match d.phase {
                Phase::Down => {
                    let st = get_mut(s);
                    st.holding = true;
                    st.started_at = tx.clock();
                    st.progress = 0.02;
                    st.fired_this_press = false;
                    tx.instant();
                    tx.wake_after(Duration::from_millis(16));
                }
                Phase::Move => {
                    // Cancel hold if pointer strays outside the button bounds
                    let inside = d.x >= -8.0 && d.x <= d.w + 8.0 && d.y >= -8.0 && d.y <= d.h + 8.0;
                    if !inside {
                        let st = get_mut(s);
                        st.holding = false;
                        st.progress = 0.0;
                        tx.animated();
                        tx.motion(CONTROL_SPRING);
                    }
                }
                Phase::Up => {
                    let st = get_mut(s);
                    st.holding = false;
                    st.progress = 0.0;
                    st.fired_this_press = false;
                    tx.animated();
                    tx.motion(CONTROL_SPRING);
                }
            })
    }
}

/// Safety Tier pill badge with semantic OKLCH colors (`Safe · Regenerable`, `Review · Opt-In`, `Protected · Inspect Only`).
pub fn safety_pill(tier: SafetyTier, in_use_by: Option<&str>, compact: bool, k: &Kit) -> El {
    if let Some(proc_name) = in_use_by {
        let (bg, fg, border) = if k.dark {
            (
                oklcha(0.32, 0.09, 32.0, 0.36),
                oklch(0.82, 0.14, 32.0),
                oklcha(0.72, 0.13, 32.0, 0.32),
            )
        } else {
            (
                oklcha(0.94, 0.05, 32.0, 0.92),
                oklch(0.45, 0.14, 32.0),
                oklcha(0.50, 0.13, 32.0, 0.28),
            )
        };
        let label = if compact {
            format!("In Use · {proc_name}")
        } else {
            format!("Active · {proc_name}")
        };
        return row()
            .h(20.0)
            .px(7.0)
            .gap(4.0)
            .radius(5.0)
            .center_y()
            .fill(bg)
            .border(1.0, border)
            .child(icon(&glyphs::LOCK, 11.0).color(fg))
            .child(
                text(label)
                    .font(tabular_small_medium(k))
                    .color(fg)
                    .truncate(),
            );
    }

    let (bg, fg, border) = safety_colors(tier, k.dark);
    let label = if compact {
        tier.short_label()
    } else {
        tier.badge_label()
    };
    row()
        .h(20.0)
        .px(7.0)
        .radius(5.0)
        .center_y()
        .fill(bg)
        .border(1.0, border)
        .child(text(label).font(tabular_small_medium(k)).color(fg).truncate())
}
