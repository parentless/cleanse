//! Visual Theme, OKLCH Tokens, Tabular Typography & Geometric Icons (`ui/theme.rs`).
//!
//! Synthesizes `apple-design` and `emil-design-eng`:
//! - Perceptually uniform OKLCH category colors calibrated for both Dark and Light appearances
//! - Hairline borders via low-alpha contrast (`border_weak`), never harsh solid dividers
//! - Tabular numerals (`'tnum' 1`) across every byte readout, badge, age, and percentage
//! - Tight display tracking (`-0.025em`) and crisp caption uppercase tracking

use rune::{Color, Icon, Spring, TextStyle, oklch, oklcha};
use rune_kit::{Kit, PaletteSeed};

use crate::diskmap::Class;
use crate::model::{Category, SafetyTier};

/// Critically damped UI spring (`apple-design`: default `bounce = 0.0`, fast `< 280ms` settle).
pub const UI_SPRING: Spring = Spring::new(0.24, 0.0);
/// Snappy control spring (`180ms`, zero bounce).
pub const CONTROL_SPRING: Spring = Spring::new(0.18, 0.0);
/// Subtle physical release spring for slider snap-back (`0.26s`, `0.10` gentle bounce).
pub const RELEASE_SPRING: Spring = Spring::new(0.26, 0.10);

/// Construct the Cleanse `Kit` seeded with calm slate-cobalt surfaces in Dark and crisp warm paper in Light.
pub fn clearance_kit(dark: bool) -> Kit {
    let seed = if dark {
        PaletteSeed {
            bg: oklch(0.145, 0.008, 255.0),
            fg: oklch(0.955, 0.005, 255.0),
            accent: oklch(0.66, 0.155, 252.0),
        }
    } else {
        PaletteSeed {
            bg: oklch(0.982, 0.003, 255.0),
            fg: oklch(0.17, 0.010, 255.0),
            accent: oklch(0.53, 0.165, 252.0),
        }
    };
    let mut k = Kit::from_palette(seed);
    k.motion.control = CONTROL_SPRING;
    k
}

/// Category foreground dot/bar color in OKLCH, tuned so all 7 categories sit at harmonious lightness.
pub fn category_accent(cat: Category, dark: bool) -> Color {
    let h = cat.hue();
    if dark {
        oklch(0.72, 0.145, h)
    } else {
        oklch(0.54, 0.155, h)
    }
}

/// Subtle tinted surface fill for Spatial Bento treemap cards.
pub fn category_surface(cat: Category, dark: bool, selected: bool) -> Color {
    let h = cat.hue();
    if dark {
        if selected {
            oklcha(0.30, 0.065, h, 0.92)
        } else {
            oklcha(0.225, 0.032, h, 0.88)
        }
    } else if selected {
        oklcha(0.92, 0.048, h, 0.96)
    } else {
        oklcha(0.958, 0.020, h, 0.92)
    }
}

/// Border color for Spatial Bento cards (`1px` subtle chromatic rim).
pub fn category_border(cat: Category, dark: bool, selected: bool) -> Color {
    let h = cat.hue();
    if dark {
        if selected {
            oklcha(0.74, 0.14, h, 0.65)
        } else {
            oklcha(0.68, 0.08, h, 0.22)
        }
    } else if selected {
        oklcha(0.52, 0.15, h, 0.55)
    } else {
        oklcha(0.45, 0.07, h, 0.18)
    }
}

/// Colors `(bg, fg, border)` for a [`SafetyTier`] pill.
pub fn safety_colors(tier: SafetyTier, dark: bool) -> (Color, Color, Color) {
    match (tier, dark) {
        (SafetyTier::SafeRegenerable, true) => (
            oklcha(0.32, 0.08, 152.0, 0.34),
            oklch(0.80, 0.14, 152.0),
            oklcha(0.72, 0.13, 152.0, 0.32),
        ),
        (SafetyTier::SafeRegenerable, false) => (
            oklcha(0.93, 0.05, 152.0, 0.85),
            oklch(0.41, 0.13, 152.0),
            oklcha(0.48, 0.12, 152.0, 0.25),
        ),
        (SafetyTier::ReviewRecommended, true) => (
            oklcha(0.33, 0.08, 75.0, 0.34),
            oklch(0.82, 0.14, 75.0),
            oklcha(0.75, 0.13, 75.0, 0.32),
        ),
        (SafetyTier::ReviewRecommended, false) => (
            oklcha(0.94, 0.05, 75.0, 0.90),
            oklch(0.45, 0.13, 72.0),
            oklcha(0.52, 0.12, 72.0, 0.28),
        ),
        (SafetyTier::ProtectedInspectOnly, true) => (
            oklcha(0.28, 0.03, 265.0, 0.45),
            oklch(0.75, 0.04, 265.0),
            oklcha(0.65, 0.04, 265.0, 0.26),
        ),
        (SafetyTier::ProtectedInspectOnly, false) => (
            oklcha(0.93, 0.015, 265.0, 0.90),
            oklch(0.43, 0.03, 265.0),
            oklcha(0.48, 0.03, 265.0, 0.22),
        ),
    }
}

/// `(bg, fg, border)` for a deletability [`Class`] pill. Safe = green, Review = amber,
/// Yours = blue, Required = slate, so the meaning is readable without the label.
pub fn class_colors(class: Class, dark: bool) -> (Color, Color, Color) {
    match (class, dark) {
        (Class::Safe, _) => safety_colors(SafetyTier::SafeRegenerable, dark),
        (Class::Review, _) => safety_colors(SafetyTier::ReviewRecommended, dark),
        (Class::Required, _) => safety_colors(SafetyTier::ProtectedInspectOnly, dark),
        (Class::Yours, true) => (
            oklcha(0.32, 0.08, 255.0, 0.34),
            oklch(0.80, 0.12, 255.0),
            oklcha(0.72, 0.12, 255.0, 0.32),
        ),
        (Class::Yours, false) => (
            oklcha(0.93, 0.04, 255.0, 0.90),
            oklch(0.42, 0.14, 255.0),
            oklcha(0.48, 0.12, 255.0, 0.26),
        ),
    }
}

/// Solid bar/dot color for a [`Class`].
pub fn class_accent(class: Class, dark: bool) -> Color {
    let (h, c) = match class {
        Class::Safe => (152.0, 0.15),
        Class::Review => (75.0, 0.14),
        Class::Yours => (255.0, 0.14),
        Class::Required => (265.0, 0.03),
    };
    if dark {
        oklch(if class == Class::Required { 0.52 } else { 0.72 }, c, h)
    } else {
        oklch(if class == Class::Required { 0.62 } else { 0.55 }, c, h)
    }
}

// ---------------------------------------------------------------------------
// Typography helpers (`apple-design` optical tracking + `'tnum' 1` everywhere)
// ---------------------------------------------------------------------------

pub fn hero_metric_font(k: &Kit) -> TextStyle {
    TextStyle {
        size: 30.0,
        weight: 650.0,
        line_height: 34.0,
        tracking: -0.75,
        ..k.ty.body_medium
    }
    .features("'tnum' 1, 'cv01' 1")
}

pub fn card_metric_font(k: &Kit) -> TextStyle {
    TextStyle {
        size: 19.0,
        weight: 620.0,
        line_height: 23.0,
        tracking: -0.35,
        ..k.ty.body_medium
    }
    .features("'tnum' 1")
}

pub fn tabular_ui(k: &Kit) -> TextStyle {
    k.ty.ui.features("'tnum' 1")
}

pub fn tabular_ui_medium(k: &Kit) -> TextStyle {
    k.ty.ui_medium.features("'tnum' 1")
}

pub fn tabular_small(k: &Kit) -> TextStyle {
    k.ty.small.features("'tnum' 1")
}

pub fn tabular_small_medium(k: &Kit) -> TextStyle {
    k.ty.small_medium.features("'tnum' 1")
}

// ---------------------------------------------------------------------------
// Crisp 16x16 Geometric Vector Icons
// ---------------------------------------------------------------------------

pub static ICON_GRID: Icon = Icon {
    name: "clr-grid",
    view: 16.0,
    body: r#"<rect x="2" y="2" width="5.2" height="5.2" rx="1.2" stroke="currentColor" stroke-width="1.3"/><rect x="8.8" y="2" width="5.2" height="5.2" rx="1.2" stroke="currentColor" stroke-width="1.3"/><rect x="2" y="8.8" width="5.2" height="5.2" rx="1.2" stroke="currentColor" stroke-width="1.3"/><rect x="8.8" y="8.8" width="5.2" height="5.2" rx="1.2" stroke="currentColor" stroke-width="1.3"/>"#,
};

pub static ICON_LIST: Icon = Icon {
    name: "clr-list",
    view: 16.0,
    body: r#"<path d="M2.5 4.2H13.5M2.5 8H13.5M2.5 11.8H10.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#,
};

pub static ICON_SHIELD: Icon = Icon {
    name: "clr-shield",
    view: 16.0,
    body: r#"<path d="M8 1.8L13.2 3.8V7.8C13.2 11.1 10.9 13.5 8 14.4C5.1 13.5 2.8 11.1 2.8 7.8V3.8L8 1.8Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><path d="M5.8 8.1L7.3 9.6L10.4 6.4" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static ICON_VAULT: Icon = Icon {
    name: "clr-vault",
    view: 16.0,
    body: r#"<rect x="2.2" y="3" width="11.6" height="10" rx="1.8" stroke="currentColor" stroke-width="1.3"/><circle cx="8" cy="8" r="2.2" stroke="currentColor" stroke-width="1.3"/><path d="M8 4.6V5.8M8 10.2V11.4M4.6 8H5.8M10.2 8H11.4" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/>"#,
};

pub static ICON_REFRESH: Icon = Icon {
    name: "clr-refresh",
    view: 16.0,
    body: r#"<path d="M13.2 8A5.2 5.2 0 1 1 11.7 4.3M13.3 2.6V5.3H10.6" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static ICON_CODE: Icon = Icon {
    name: "clr-code",
    view: 16.0,
    body: r#"<path d="M5.5 5L2.5 8L5.5 11M10.5 5L13.5 8L10.5 11M8.8 3.5L7.2 12.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static ICON_BOX: Icon = Icon {
    name: "clr-box",
    view: 16.0,
    body: r#"<path d="M8 1.8L13.5 4.8V11.2L8 14.2L2.5 11.2V4.8L8 1.8Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><path d="M2.5 4.8L8 7.8L13.5 4.8M8 7.8V14.2" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/>"#,
};

pub static ICON_LAYERS: Icon = Icon {
    name: "clr-layers",
    view: 16.0,
    body: r#"<path d="M8 2.2L13.8 5.2L8 8.2L2.2 5.2L8 2.2Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><path d="M2.2 8.4L8 11.4L13.8 8.4M2.2 11.2L8 14.2L13.8 11.2" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static ICON_DISC: Icon = Icon {
    name: "clr-disc",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="5.6" stroke="currentColor" stroke-width="1.3"/><circle cx="8" cy="8" r="1.8" stroke="currentColor" stroke-width="1.3"/>"#,
};

pub static ICON_GHOST: Icon = Icon {
    name: "clr-ghost",
    view: 16.0,
    body: r#"<path d="M4 13.5V7A4 4 0 0 1 12 7V13.5L10.2 12L8 13.5L5.8 12L4 13.5Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>"#,
};

pub static ICON_TRASH: Icon = Icon {
    name: "clr-trash",
    view: 16.0,
    body: r#"<path d="M3 4.5H13M6 4.5V3H10V4.5M4.4 4.5L5 13.2H11L11.6 4.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static ICON_RADAR: Icon = Icon {
    name: "clr-radar",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="5.6" stroke="currentColor" stroke-width="1.3"/><circle cx="8" cy="8" r="2.4" stroke="currentColor" stroke-width="1.1"/><path d="M8 8L12 4" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#,
};

pub static ICON_UNDO: Icon = Icon {
    name: "clr-undo",
    view: 16.0,
    body: r#"<path d="M3.2 6.5H9.8A3.4 3.4 0 1 1 9.8 13.3H6.5M3.2 6.5L5.8 3.9M3.2 6.5L5.8 9.1" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static ICON_SUN: Icon = Icon {
    name: "clr-sun",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="3" stroke="currentColor" stroke-width="1.3"/><path d="M8 1.8V3.2M8 12.8V14.2M1.8 8H3.2M12.8 8H14.2M3.6 3.6L4.6 4.6M11.4 11.4L12.4 12.4M12.4 3.6L11.4 4.6M4.6 11.4L3.6 12.4" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#,
};

pub static ICON_MOON: Icon = Icon {
    name: "clr-moon",
    view: 16.0,
    body: r#"<path d="M12.8 9.8A5.2 5.2 0 0 1 6.2 3.2A5.3 5.3 0 1 0 12.8 9.8Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>"#,
};

pub static ICON_FOLDER: Icon = Icon {
    name: "clr-folder",
    view: 16.0,
    body: r#"<path d="M2 4.6A1.5 1.5 0 0 1 3.5 3.1H6.2L7.7 4.7H12.5A1.5 1.5 0 0 1 14 6.2V11.6A1.5 1.5 0 0 1 12.5 13.1H3.5A1.5 1.5 0 0 1 2 11.6V4.6Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>"#,
};

pub static ICON_FILE: Icon = Icon {
    name: "clr-file",
    view: 16.0,
    body: r#"<path d="M4 2.2H9.1L12 5.1V13.2A.6.6 0 0 1 11.4 13.8H4.6A.6.6 0 0 1 4 13.2V2.2Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><path d="M9 2.3V5.2H11.9" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/>"#,
};

pub static ICON_MAP: Icon = Icon {
    name: "clr-map",
    view: 16.0,
    body: r#"<rect x="2" y="2.6" width="12" height="10.8" rx="1.7" stroke="currentColor" stroke-width="1.3"/><path d="M7.2 2.6V13.4M7.2 8H14" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/>"#,
};

pub static ICON_CHEVRON: Icon = Icon {
    name: "clr-chevron",
    view: 16.0,
    body: r#"<path d="M6 3.5L10.5 8L6 12.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub fn category_icon(cat: Category) -> &'static Icon {
    match cat {
        Category::DevArtifacts => &ICON_CODE,
        Category::ToolchainHoards => &ICON_BOX,
        Category::SystemCaches => &ICON_LAYERS,
        Category::InstallerGhosts => &ICON_DISC,
        Category::OrphanedLeftovers => &ICON_GHOST,
        Category::TrashBin => &ICON_TRASH,
        Category::Heavyweights => &ICON_RADAR,
    }
}
