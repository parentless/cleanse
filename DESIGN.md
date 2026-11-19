# Cleanse — Design & Motion System (`DESIGN.md`)

This document encodes the craft, visual hierarchy, safety guarantees, and motion rules for **Cleanse**, derived from Emil Kowalski's UI skills (`aiforui.dev/skills`) and Apple's Fluid Interface principles, translated directly onto **Rune** (`anomalyco/rune` + `rune-kit`).

Before building or modifying any UI in this repository, read this file.

---

## 1. Product Philosophy & Safety Constitution

- **Agency & Forgiveness over Nag Dialogs (`apple-design` §16):** Never train the user to click through repetitive modal confirmation boxes. Instead:
  1. Every discovered item carries an explicit **Safety Cleanse Tier** (`Safe Regenerable`, `Review Suggested`, `Protected · Inspect Only`) and an inline **Why it's safe / Rebuild command** (`cargo build`, `pnpm install`, `App regenerates on launch`).
  2. Necessary OS, credential, personal document, active-process, and `.git` paths are blocked by a constitutional path guard (`src/safety.rs`) that cannot be overridden from the UI.
  3. Default reclamation stages items into a **Reversible Quarantine Vault** (`~/.clearance-vault/<sweep-id>/manifest.json`) via instant same-volume `rename`, paired with an inline **Undo** action in the `ToastStack` and Vault view, plus a **Dry-Run Preview** mode and **Move to macOS Trash (`~/.Trash`)** option.

## 2. Surfaces & Depth (`apple-design` §12)

- **Layered OKLCH hierarchy:** Root canvas (`bg_deep` / `bg_base`), raised bento cards (`layer_01` with subtle `0.5pt` hairline ring + soft two-layer elevation shadow, never heavy opaque 1px borders), and interactive chips.
- **Translucent floating chrome (`El::backdrop_blur(10.0)`):** The bottom Sweep Action Dock floats over scrolling findings with backdrop blur and `.scroll_edge_fade(18.0)` on the scroll viewport — never a hard 1px divider cutting off glyphs.
- **Native macOS chrome:** Full-size content view (`Chrome { full_size_content: true, traffic_lights: Some((16.0, 22.0)), drag_regions: true }`) with dedicated window-drag header clearance.

## 3. Typography & Numeric Discipline (`apple-design` §15, `break-ui`)

- **Size-specific tracking:**
  - Hero storage figures (`32–40pt`): negative tracking (`-0.8pt` to `-1.1pt`), tight leading (`1.08×`), optical sizing enabled.
  - Card titles (`15–17pt`): slight negative tracking (`-0.2pt`), medium-semibold weight (`560–620`).
  - Body & row labels (`13pt`): neutral tracking (`0.0pt`), comfortable leading (`18pt`).
  - Section eyebrows & clearance badges (`11pt`): slight positive tracking (`+0.4pt`), semibold (`600`).
- **Tabular figures (`'tnum' 1`) everywhere numbers appear:** Every byte size, percentage, file count, age badge, and live scan counter uses `.features("'tnum' 1")` or `RollingText` so columns align and digits never jitter during scanning or slider drags.
- **Path & label resilience (`break-ui`):**
  - File paths and bundle identifiers use `.truncate_middle()` (preserving both the root folder and leaf filename) paired with `.help(full_path)` tooltips and `.selectable(true)` in the inspector.
  - Counts always pluralize grammatically (`1 item` vs `2 items`, `1 dormant project` vs `4 dormant projects`).
  - Flex rows containing dynamic text always set `.shrink()` / `.min_w(0.0)` on the text column so trailing size badges and checkboxes can never be pushed off-screen.

## 4. Motion & Choreography Rules (`emil-design-eng`, `animate`, `choreograph`, `remove-animations`)

### Frequency Gate (What NEVER Animates)
- **Keyboard shortcuts & Command Palette (`⌘K`, `⌘1–5`, `↑/↓` row navigation, `Space` toggle):** **Zero spatial animation (`0ms`, `.instant_changes()` / `Tx::instant()`).** High-frequency actions (100+/day) must feel instantaneous like Raycast.
- **Sidebar & view switching, dataset switching (`Demo / Worst Case`):** No slide transitions between peer tabs; content swaps immediately or with a subtle crisp state update.
- **Functional table rows on filter/sort:** Virtualized rows update immediately without staggered row entrances.

### Justified Motion (What DOES Animate & Exact Ingredients)
- **Button & tile press feedback (`animate`):** `.press_scale(0.97)` on interactive buttons and spatial treemap tiles — responds on `pointer-down`, springs back cleanly on release.
- **Number ticker (`animation-vocabulary`, `pick-ui-library`):** `rune_kit::RollingText` on total Reclaimable Space and Freestanding Headroom (`Spring::new(0.28, 0.0)`, critically damped, zero bounce) — animates only when values actually change, never counting up from zero on initial mount.
- **Shared-element category zoom (`El::matched`):** Clicking a category tile in the Spatial Storage Map flies it smoothly into the Category Deep-Dive header (`Spring::new(0.28, 0.0)`), and `Esc` / Back returns along the exact symmetric path.
- **Asymmetric Hold-to-Sweep (`emil-design-eng`, `gestures`):**
  - **Hold phase (user deciding):** `1.4s linear` progress fill across the sweep button (`Live<[f32; 4]>` / linear clock progress) + `.press_scale(0.97)`. Honest linear time representation.
  - **Cancel / early release (system responding):** Snaps back fast (`180ms` critically damped spring).
- **Direct-manipulation Dormancy Horizon Slider (`gestures`, `apple-design`):**
  - **Track phase (`Phase::Move`):** `1:1` pointer tracking with grab-offset preservation and `tx.instant()` (zero spring lag while dragging) + rubber-band resistance past bounds (`rubberband(overshoot, width, 0.55)`).
  - **Release / click-track phase:** Critically damped spring (`Spring::new(0.24, 0.0)`).
- **Toast notifications (`ask-sonner`):** Single root-mounted `ToastStack` at bottom-right; enters and exits symmetrically along Y, pauses timers on hover/expand, updates scan/clean status in-place by key, and surfaces 1-click **Undo**.
- **Presence & Gap Closing (`choreograph`):** Never animate from `scale(0)` — overlays use `Transition::scale(0.96)` (0.96 scale + opacity). Cleaned rows release space immediately while fading so siblings glide together rather than queuing.
- **Reduced Motion (`apple-design` §14):** Honors `cx.system_reduced_motion()` (plus an in-app override toggle for testing): collapses spatial flights, press scale, and slider springs into calm `150ms` opacity/color transitions (`REDUCED_FADE`).

## 5. Performance Budget (`animation-performance`)

- Target **120 Hz (`8.3ms` frame budget)** on Apple Silicon.
- Disk scanning runs strictly off the UI thread on worker threads, publishing live progress via lock-free/coalesced channels polled at frame boundaries without blocking `view()`.
- Large file & artifact lists use `rune_kit::Table` virtualization (`virtual_rows`) so 5,000+ items cost `<0.5ms` per frame.
- Unchanged tiles wrap in `rune::memo(...)` keyed on minimal state tuples.

---

## 6. Disk Map (whole-Mac analysis)

`⌘2` opens **Disk Map**: where every byte on the Mac lives, and what can be deleted. Code: `src/diskmap.rs` (scanner + rules), `src/ui/diskmap_view.rs` (surface).

- **Scanner:** walks the whole Data volume plus the sealed system volume in parallel (bounded thread permits, never follows symlinks, never crosses mounts). Measures *physical* bytes (`st_blocks * 512`), counts hard links once, and skips firmlink paths from `/usr/share/firmlinks` so nothing is counted twice. A 2.5M-file Mac scans in ~40 s; finished roots stream into the UI as they complete.
- **Bounded tree:** each folder keeps its 40 largest children (≥ 512 KB) and folds the rest into one "N smaller items" row, so totals always add up and memory stays small.
- **Four verdicts on every row** (`Class`): **Safe to delete** (regenerates), **Delete if unused** (apps, old installers, simulators), **Your data** (never touched), **Required by macOS** (cannot be deleted). Rules live in `classify()`; anything unrecognised defaults to *Your data*. Required can never be weakened by a name rule, and nothing inside an `.app` bundle is judged separately.
- **Held by macOS:** the gap between "used" and "files we can read" is shown honestly (APFS snapshots, staged updates, Preboot/Recovery/VM, folders blocked without Full Disk Access) with a one-click path to grant access.
- **Four lenses:** Explorer (drill down, filter by verdict), Can Delete (flat, non-overlapping list of whole deletable units), Biggest Files (with likely-duplicate detection), By Type.
- **Read-only by design.** Disk Map explains; it never deletes. Rows that the regular scanner also found offer "Open in Findings", which uses the Quarantine Vault with Undo.
