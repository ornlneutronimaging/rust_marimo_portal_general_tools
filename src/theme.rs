//! Coefficient design-system tokens and egui theming.
//!
//! Values follow ORNL's "Coefficient" LLM knowledge base (see
//! `llms-full.txt.rtf`): the ORNL Green brand (`#0F8723`) as the primary
//! interactive color, neutral surfaces as the foundation for backgrounds, and
//! the semantic status roles (Success / Danger / Warning / Info). The neutral
//! ramp comes in two variants: dark surfaces with light text — the design
//! system's "dark theme parity" — and a light counterpart, selected by the
//! user's saved theme preference.
//!
//! Guidance applied from the knowledge base:
//!  - Primary color denotes interactivity / the most important action.
//!  - Neutrals are the foundation; color is reserved for emphasis & status.
//!  - Type ramp establishes hierarchy: Header (structure) vs Body (reading).
//!  - Accessibility: white-on-`#0F8723` clears WCAG AA (~4.6:1); focus visible.
//!
//! The light/dark preference lives in one file
//! (`~/.config/venus_rust_tools/theme`, containing `dark` or `light`) so
//! switching the theme in any of the VENUS rust tools switches all of them —
//! the next time each one starts. Dark is the default: it is what every tool
//! shipped with before the preference existed.

// This is a design-token palette: the full semantic set (e.g. `INFO`) and the
// complete spacing scale are defined for consistent reuse even where not yet
// referenced by the current screens.
#![allow(dead_code)]

use eframe::egui::{self, Color32, Stroke};
use std::path::PathBuf;

// --- Brand / primary (ORNL Green) ---
/// `--primary-rich`: brand green. Header background & primary button fill.
pub const PRIMARY_RICH: Color32 = Color32::from_rgb(0x0F, 0x87, 0x23);
/// Interactive/hover shade of primary.
pub const PRIMARY: Color32 = Color32::from_rgb(0x12, 0x9B, 0x2B);
/// `--primary-text-emphasis`: bright green for selected/interactive text on dark.
pub const PRIMARY_STRONG: Color32 = Color32::from_rgb(0x3D, 0xD1, 0x60);

// --- Neutral surfaces (dark) ---
/// `--surface-base`: page background.
pub const SURFACE_BASE: Color32 = Color32::from_rgb(0x15, 0x17, 0x1C);
/// `--surface-weak`: raised panels.
pub const SURFACE_WEAK: Color32 = Color32::from_rgb(0x1D, 0x20, 0x27);
/// Container surface for list boxes and input fields.
pub const SURFACE_CONTAINER: Color32 = Color32::from_rgb(0x23, 0x27, 0x30);
/// `--neutral-border-subtle`: default borders & dividers.
pub const BORDER_SUBTLE: Color32 = Color32::from_rgb(0x3A, 0x3F, 0x4B);

// --- Neutral surfaces (light counterparts) ---
/// `--surface-base`, light: page background.
pub const SURFACE_BASE_LIGHT: Color32 = Color32::from_rgb(0xF7, 0xF8, 0xFA);
/// `--surface-weak`, light: raised panels.
pub const SURFACE_WEAK_LIGHT: Color32 = Color32::from_rgb(0xEB, 0xED, 0xF1);
/// Container surface for list boxes and input fields, light.
pub const SURFACE_CONTAINER_LIGHT: Color32 = Color32::WHITE;
/// `--neutral-border-subtle`, light: default borders & dividers.
pub const BORDER_SUBTLE_LIGHT: Color32 = Color32::from_rgb(0xC9, 0xCE, 0xD6);

// --- Neutral text ---
/// `--neutral-text-strong`: primary copy (dark mode).
pub const TEXT_STRONG: Color32 = Color32::from_rgb(0xF2, 0xF3, 0xF5);
/// `--neutral-text-emphasis`: secondary text / metadata (dark mode).
pub const TEXT_EMPHASIS: Color32 = Color32::from_rgb(0xA8, 0xB0, 0xBD);
/// `--neutral-text-emphasis`, light: secondary text / metadata.
pub const TEXT_EMPHASIS_LIGHT: Color32 = Color32::from_rgb(0x5A, 0x64, 0x72);
/// `--text-white`: foreground on the branded (green) header & primary button.
pub const TEXT_WHITE: Color32 = Color32::WHITE;

// --- Semantic status ---
pub const SUCCESS: Color32 = Color32::from_rgb(0x2E, 0xA0, 0x43);
pub const DANGER: Color32 = Color32::from_rgb(0xE5, 0x48, 0x4D);
pub const WARNING: Color32 = Color32::from_rgb(0xD1, 0x86, 0x16);
pub const INFO: Color32 = Color32::from_rgb(0x3B, 0x82, 0xF6);

// --- Spacing scale ---
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_SM: f32 = 8.0;
pub const SPACE_MD: f32 = 12.0;
pub const SPACE_LG: f32 = 16.0;

// --- Mode-aware tokens: the neutral ramp of whichever mode is active ---

/// `--surface-base` for the active mode (equals the active `panel_fill`).
pub fn surface_base(visuals: &egui::Visuals) -> Color32 {
    if visuals.dark_mode { SURFACE_BASE } else { SURFACE_BASE_LIGHT }
}

/// `--surface-weak` for the active mode.
pub fn surface_weak(visuals: &egui::Visuals) -> Color32 {
    if visuals.dark_mode { SURFACE_WEAK } else { SURFACE_WEAK_LIGHT }
}

/// Container surface for the active mode.
pub fn surface_container(visuals: &egui::Visuals) -> Color32 {
    if visuals.dark_mode { SURFACE_CONTAINER } else { SURFACE_CONTAINER_LIGHT }
}

/// `--neutral-border-subtle` for the active mode.
pub fn border_subtle(visuals: &egui::Visuals) -> Color32 {
    if visuals.dark_mode { BORDER_SUBTLE } else { BORDER_SUBTLE_LIGHT }
}

/// `--neutral-text-emphasis` for the active mode.
pub fn text_emphasis(visuals: &egui::Visuals) -> Color32 {
    if visuals.dark_mode { TEXT_EMPHASIS } else { TEXT_EMPHASIS_LIGHT }
}

/// Install the Coefficient theme (tokens, type ramp, spacing) onto the
/// context, styling both modes; `Context::set_theme` picks the active one.
/// Call once at startup; egui persists the styles across frames.
pub fn apply(ctx: &egui::Context) {
    // Font sizes and spacing are left at egui's defaults (matching the original
    // portal's density); hierarchy comes from weight (strong), color tokens, and
    // the branded header rather than an enlarged type ramp or looser spacing. The
    // design system specifies Mulish/Roboto, but we keep the default sans to
    // avoid bundling a font.

    for (theme, dark) in [(egui::Theme::Dark, true), (egui::Theme::Light, false)] {
        let mut style = (*ctx.style_of(theme)).clone();

        let mut v = if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        v.panel_fill = if dark { SURFACE_BASE } else { SURFACE_BASE_LIGHT };
        v.window_fill = v.panel_fill;
        // text-edit background
        v.extreme_bg_color = if dark { SURFACE_CONTAINER } else { SURFACE_CONTAINER_LIGHT };
        // Selected state uses primary (design system: primary for selected states).
        v.selection.bg_fill = PRIMARY;
        v.selection.stroke = Stroke::new(1.0, TEXT_WHITE);
        // The bright green reads on dark surfaces only; light mode links use
        // the rich brand green instead.
        v.hyperlink_color = if dark { PRIMARY_STRONG } else { PRIMARY_RICH };
        if dark {
            // The widget-surface overrides restyle only the dark ramp; egui's
            // stock light widgets already sit on light neutral surfaces.
            v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_STRONG);
            v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_STRONG);
            v.widgets.inactive.bg_fill = SURFACE_WEAK;
            v.widgets.inactive.weak_bg_fill = SURFACE_WEAK;
            v.widgets.hovered.bg_fill = SURFACE_CONTAINER;
            v.widgets.hovered.weak_bg_fill = SURFACE_CONTAINER;
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
        }
        style.visuals = v;

        ctx.set_style_of(theme, style);
    }
}

/// A framed container surface (neutral fill + subtle border) used for the IPTS
/// and application lists — separation via surface & border tokens.
pub fn container_frame(visuals: &egui::Visuals) -> egui::Frame {
    egui::Frame::new()
        .fill(surface_container(visuals))
        .stroke(Stroke::new(1.0, border_subtle(visuals)))
        .corner_radius(4.0)
        .inner_margin(SPACE_XS)
}

/// A section heading (Header type role): weight structures content at the
/// default body size (no enlargement); the color is the active style's
/// strong text.
pub fn section_heading(text: &str) -> egui::RichText {
    egui::RichText::new(text).strong()
}

/// The single primary action button: ORNL Green fill with a white, title-case
/// label. Per the design system, reserve this for the most important action.
pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.to_owned())
            .color(TEXT_WHITE)
            .strong(),
    )
    .fill(PRIMARY_RICH)
    .min_size(egui::vec2(240.0, 36.0))
}

// --- Light / dark preference, shared by every VENUS rust tool ---

/// The preference file, under `$XDG_CONFIG_HOME` (or `~/.config`).
fn pref_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
        })?;
    Some(base.join("venus_rust_tools").join("theme"))
}

/// The saved preference, or dark when there is none (or it is unreadable).
pub fn load() -> egui::Theme {
    match pref_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(s) if s.trim().eq_ignore_ascii_case("light") => egui::Theme::Light,
        _ => egui::Theme::Dark,
    }
}

/// Persist the preference. Best effort: a read-only home directory only
/// costs the user their choice on the next start, not an error dialog.
pub fn save(theme: egui::Theme) {
    let Some(path) = pref_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        path,
        match theme {
            egui::Theme::Light => "light\n",
            egui::Theme::Dark => "dark\n",
        },
    );
}

/// A sun / moon button that flips the theme of the whole application and
/// saves the choice for every VENUS rust tool. Drop it anywhere in a toolbar.
pub fn toggle_button(ui: &mut egui::Ui) {
    let (icon, tip, next) = match ui.ctx().theme() {
        egui::Theme::Dark => ("☀", "Switch to the light theme", egui::Theme::Light),
        egui::Theme::Light => ("🌙", "Switch to the dark theme", egui::Theme::Dark),
    };
    if ui
        .button(icon)
        .on_hover_text(format!("{tip} (applies to all the VENUS rust tools)"))
        .clicked()
    {
        ui.ctx().set_theme(next);
        save(next);
    }
}
