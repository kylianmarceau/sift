//! Icons compiled into the binary and served to GPUI's SVG renderer.
//!
//! GPUI draws an SVG as a mask in the element's text colour, so the files
//! only need shapes, not colours. Each is rasterised once per size and kept
//! in the texture atlas.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

pub const SEARCH: &str = "icons/search.svg";
pub const CLIPBOARD: &str = "icons/clipboard.svg";
pub const FOLDER: &str = "icons/folder.svg";

const FILES: &[(&str, &[u8])] = &[
    (SEARCH, include_bytes!("../assets/icons/search.svg")),
    (CLIPBOARD, include_bytes!("../assets/icons/clipboard.svg")),
    (FOLDER, include_bytes!("../assets/icons/folder.svg")),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(FILES
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(FILES
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_loads_and_is_svg() {
        for (name, _) in FILES {
            let bytes = Assets.load(name).expect("load").expect("present");
            assert!(bytes.starts_with(b"<svg"), "{name}");
        }
        assert!(Assets.load("icons/missing.svg").expect("load").is_none());
        assert_eq!(Assets.list("icons/").expect("list").len(), FILES.len());
    }
}
