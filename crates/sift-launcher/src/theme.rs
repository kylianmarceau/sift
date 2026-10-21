//! Light and dark colours, following the system appearance.

use gpui::{Hsla, WindowAppearance, rgb, rgba};

#[derive(Debug, Clone, Copy)]
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
}

impl Theme {
    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
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
        }
    }
}
