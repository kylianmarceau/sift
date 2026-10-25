//! Colours for the launcher and the settings window.
//!
//! One set of tokens per appearance (light, dark) and contrast level,
//! following the system appearance, the accent colour from System Settings
//! and the accessibility display settings (Increase Contrast, Differentiate
//! Without Color). Surfaces are opaque: no blur or transparency, which
//! would cost GPU work on every frame.

use gpui::{Hsla, Rgba, WindowAppearance, rgb, rgba};
use sift_macos::a11y::{DisplayOptions, accent_color, display_options};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// The window background.
    pub background: Hsla,
    /// The footer bar and other secondary surfaces.
    pub surface: Hsla,
    /// Cards in the settings window.
    pub card: Hsla,
    /// Hairlines between areas and around cards.
    pub divider: Hsla,
    pub text: Hsla,
    /// Secondary text: subtitles, descriptions, footer labels.
    pub muted: Hsla,
    /// Tertiary text: placeholders, section labels, kinds.
    pub placeholder: Hsla,
    /// The highlighted result row.
    pub selected: Hsla,
    /// Rows and buttons under the pointer.
    pub hover: Hsla,
    pub accent: Hsla,
    /// Text and symbols drawn on the accent colour.
    pub on_accent: Hsla,
    /// Key caps and secondary buttons.
    pub key_background: Hsla,
    pub key_text: Hsla,
    pub error: Hsla,
    pub success: Hsla,
    /// The favorite star.
    pub favorite: Hsla,
    /// Drawn around the highlighted row when colour alone should not carry
    /// the selection (Increase Contrast, Differentiate Without Color).
    pub outline: Option<Hsla>,
}

impl Theme {
    /// The theme for `appearance`, the current display settings and the
    /// system accent colour.
    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        let mut theme = Self::with_options(appearance, &display_options());
        if let Some([r, g, b]) = accent_color() {
            theme.accent = Rgba { r, g, b, a: 1. }.into();
        }
        theme
    }

    pub fn with_options(appearance: WindowAppearance, options: &DisplayOptions) -> Self {
        let dark = matches!(
            appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        let mut theme = match (dark, options.increase_contrast) {
            (true, false) => Self::dark(),
            (true, true) => Self::dark_high_contrast(),
            (false, false) => Self::light(),
            (false, true) => Self::light_high_contrast(),
        };
        if options.differentiate_without_color && theme.outline.is_none() {
            theme.outline = Some(theme.accent);
        }
        theme
    }

    fn dark() -> Self {
        Self {
            background: rgb(0x1c1c1f).into(),
            surface: rgb(0x18181a).into(),
            card: rgb(0x232327).into(),
            divider: rgba(0xffffff13).into(),
            text: rgb(0xf4f4f6).into(),
            muted: rgba(0xebebf59e).into(),
            placeholder: rgba(0xebebf561).into(),
            selected: rgba(0xffffff16).into(),
            hover: rgba(0xffffff0b).into(),
            accent: rgb(0x0a84ff).into(),
            on_accent: rgb(0xffffff).into(),
            key_background: rgba(0xffffff17).into(),
            key_text: rgba(0xebebf5c7).into(),
            error: rgb(0xff6961).into(),
            success: rgb(0x32d74b).into(),
            favorite: rgb(0xffd60a).into(),
            outline: None,
        }
    }

    /// Stronger text, dividers and highlight, and an outlined selection.
    fn dark_high_contrast() -> Self {
        Self {
            background: rgb(0x111113).into(),
            surface: rgb(0x0b0b0c).into(),
            card: rgb(0x1c1c1f).into(),
            divider: rgba(0xffffff59).into(),
            text: rgb(0xffffff).into(),
            muted: rgba(0xebebf5e0).into(),
            placeholder: rgba(0xebebf5b3).into(),
            selected: rgba(0xffffff38).into(),
            hover: rgba(0xffffff1f).into(),
            accent: rgb(0x409cff).into(),
            // White on this lighter blue is under 3:1.
            on_accent: rgb(0x000000).into(),
            key_background: rgba(0xffffff2e).into(),
            key_text: rgb(0xffffff).into(),
            error: rgb(0xff8a84).into(),
            success: rgb(0x5ae070).into(),
            favorite: rgb(0xffe04d).into(),
            outline: Some(rgba(0xffffffd9).into()),
        }
    }

    fn light() -> Self {
        Self {
            background: rgb(0xfbfbfc).into(),
            surface: rgb(0xf2f2f4).into(),
            card: rgb(0xffffff).into(),
            divider: rgba(0x00000016).into(),
            text: rgb(0x1d1d1f).into(),
            muted: rgba(0x3c3c43c2).into(),
            placeholder: rgba(0x3c3c4394).into(),
            selected: rgba(0x0000000f).into(),
            hover: rgba(0x00000009).into(),
            accent: rgb(0x007aff).into(),
            on_accent: rgb(0xffffff).into(),
            key_background: rgba(0x00000011).into(),
            key_text: rgba(0x3c3c43cc).into(),
            error: rgb(0xd70015).into(),
            success: rgb(0x248a3d).into(),
            favorite: rgb(0xe0a100).into(),
            outline: None,
        }
    }

    fn light_high_contrast() -> Self {
        Self {
            background: rgb(0xffffff).into(),
            surface: rgb(0xebebee).into(),
            card: rgb(0xffffff).into(),
            divider: rgba(0x00000066).into(),
            text: rgb(0x000000).into(),
            muted: rgba(0x3c3c43eb).into(),
            placeholder: rgba(0x3c3c43b3).into(),
            selected: rgba(0x00000024).into(),
            hover: rgba(0x00000014).into(),
            accent: rgb(0x0040dd).into(),
            on_accent: rgb(0xffffff).into(),
            key_background: rgba(0x00000024).into(),
            key_text: rgb(0x000000).into(),
            error: rgb(0xad0011).into(),
            success: rgb(0x1a6b2e).into(),
            favorite: rgb(0xa86f00).into(),
            outline: Some(rgba(0x000000cc).into()),
        }
    }
}

/// The colour of the tile drawn behind results that have no file icon.
/// The same in light and dark, like system app icons.
pub fn tile_color(kind: sift_core::ResultKind) -> Hsla {
    use sift_core::ResultKind;
    match kind {
        ResultKind::Command => rgb(0x8e8e93),
        ResultKind::Calculator => rgb(0xff9f0a),
        ResultKind::Snippet => rgb(0xbf5af2),
        ResultKind::Clipboard => rgb(0x30b0c7),
        ResultKind::Window => rgb(0x0a84ff),
        ResultKind::Workspace => rgb(0x5e5ce6),
        ResultKind::Application | ResultKind::File | ResultKind::Folder => rgb(0x8e8e93),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_settings_strengthen_the_theme() {
        let plain = Theme::with_options(WindowAppearance::Dark, &DisplayOptions::default());
        assert!(plain.outline.is_none());
        let contrast = Theme::with_options(
            WindowAppearance::Dark,
            &DisplayOptions {
                increase_contrast: true,
                ..DisplayOptions::default()
            },
        );
        assert!(contrast.outline.is_some());
        assert!(contrast.muted.a > plain.muted.a);
        assert!(contrast.divider.a > plain.divider.a);
        assert!(contrast.selected.a > plain.selected.a);
        let no_color = Theme::with_options(
            WindowAppearance::Light,
            &DisplayOptions {
                differentiate_without_color: true,
                ..DisplayOptions::default()
            },
        );
        assert_eq!(no_color.outline, Some(no_color.accent));
    }

    /// Text must stay readable on every surface it is drawn on: WCAG AA
    /// asks for 4.5:1 for body text, 3:1 for large or secondary UI text.
    #[test]
    fn text_contrast_meets_wcag() {
        for appearance in [WindowAppearance::Dark, WindowAppearance::Light] {
            for increase_contrast in [false, true] {
                let options = DisplayOptions {
                    increase_contrast,
                    ..DisplayOptions::default()
                };
                let t = Theme::with_options(appearance, &options);
                for surface in [t.background, t.surface, t.card] {
                    let on = |fg| contrast(fg, surface);
                    assert!(on(t.text) >= 7.0, "{appearance:?} text");
                    assert!(on(t.muted) >= 4.5, "{appearance:?} muted");
                    assert!(on(t.placeholder) >= 3.0, "{appearance:?} placeholder");
                    assert!(on(t.error) >= 3.0, "{appearance:?} error");
                }
                assert!(contrast(t.on_accent, t.accent) >= 3.0);
            }
        }
    }

    /// WCAG contrast ratio of `fg` composited over the opaque `bg`.
    fn contrast(fg: Hsla, bg: Hsla) -> f32 {
        let (f, b) = (Rgba::from(fg), Rgba::from(bg));
        let mix = |x: f32, y: f32| x * f.a + y * (1. - f.a);
        let fg = [mix(f.r, b.r), mix(f.g, b.g), mix(f.b, b.b)];
        let lum = |c: [f32; 3]| {
            let lin = |v: f32| {
                if v <= 0.039_28 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
        };
        let (l1, l2) = (lum(fg), lum([b.r, b.g, b.b]));
        (l1.max(l2) + 0.05) / (l1.min(l2) + 0.05)
    }
}
