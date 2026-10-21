//! Light and dark colours, following the system appearance and the
//! accessibility display settings (Increase Contrast, Differentiate
//! Without Color).

use gpui::{Hsla, WindowAppearance, rgb, rgba};
use sift_macos::a11y::{DisplayOptions, display_options};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub background: Hsla,
    pub divider: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub placeholder: Hsla,
    pub selected: Hsla,
    pub accent: Hsla,
    pub key_background: Hsla,
    pub error: Hsla,
    /// Drawn around the highlighted row when colour alone should not carry
    /// the selection (Increase Contrast, Differentiate Without Color).
    pub outline: Option<Hsla>,
}

impl Theme {
    /// The theme for `appearance` and the current display settings.
    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        Self::with_options(appearance, &display_options())
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
            background: rgb(0x1f1f22).into(),
            divider: rgba(0xffffff14).into(),
            text: rgb(0xf2f2f4).into(),
            muted: rgba(0xebebf599).into(),
            placeholder: rgba(0xebebf55c).into(),
            selected: rgba(0xffffff1a).into(),
            accent: rgb(0x0a84ff).into(),
            key_background: rgba(0xffffff14).into(),
            error: rgb(0xff6961).into(),
            outline: None,
        }
    }

    /// Stronger text, dividers and highlight, and an outlined selection.
    fn dark_high_contrast() -> Self {
        Self {
            background: rgb(0x111113).into(),
            divider: rgba(0xffffff59).into(),
            text: rgb(0xffffff).into(),
            muted: rgba(0xebebf5e0).into(),
            placeholder: rgba(0xebebf5b3).into(),
            selected: rgba(0xffffff38).into(),
            accent: rgb(0x409cff).into(),
            key_background: rgba(0xffffff2e).into(),
            error: rgb(0xff8a84).into(),
            outline: Some(rgba(0xffffffd9).into()),
        }
    }

    fn light() -> Self {
        Self {
            background: rgb(0xf7f7f8).into(),
            divider: rgba(0x0000001a).into(),
            text: rgb(0x1d1d1f).into(),
            muted: rgba(0x3c3c4399).into(),
            placeholder: rgba(0x3c3c4366).into(),
            selected: rgba(0x0000000f).into(),
            accent: rgb(0x007aff).into(),
            key_background: rgba(0x0000000f).into(),
            error: rgb(0xd70015).into(),
            outline: None,
        }
    }

    fn light_high_contrast() -> Self {
        Self {
            background: rgb(0xffffff).into(),
            divider: rgba(0x00000066).into(),
            text: rgb(0x000000).into(),
            muted: rgba(0x3c3c43eb).into(),
            placeholder: rgba(0x3c3c43b3).into(),
            selected: rgba(0x00000024).into(),
            accent: rgb(0x0040dd).into(),
            key_background: rgba(0x00000024).into(),
            error: rgb(0xad0011).into(),
            outline: Some(rgba(0x000000cc).into()),
        }
    }
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
        let no_color = Theme::with_options(
            WindowAppearance::Light,
            &DisplayOptions {
                differentiate_without_color: true,
                ..DisplayOptions::default()
            },
        );
        assert_eq!(no_color.outline, Some(no_color.accent));
    }
}
