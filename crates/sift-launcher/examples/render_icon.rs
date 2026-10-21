//! Renders the app icon SVG into the PNG sizes of a macOS `.iconset`.
//!
//!   cargo run -p sift-launcher --example render_icon -- packaging/icon.svg OUT.iconset
//!
//! `scripts/make-icon.sh` runs this and packs the result with `iconutil`.

use std::path::{Path, PathBuf};

use resvg::{tiny_skia, usvg};

/// (file name, pixel size) for every image an `.icns` should hold.
const SIZES: &[(&str, u32)] = &[
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
];

fn main() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let (Some(svg), Some(out)) = (args.next(), args.next()) else {
        return Err("usage: render_icon ICON.svg OUT.iconset".into());
    };
    let (svg, out) = (PathBuf::from(svg), PathBuf::from(out));
    let data = std::fs::read(&svg).map_err(|e| format!("{}: {e}", svg.display()))?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
        .map_err(|e| format!("{}: {e}", svg.display()))?;
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    for (name, px) in SIZES {
        render(&tree, *px, &out.join(name))?;
    }
    println!("wrote {} images to {}", SIZES.len(), out.display());
    Ok(())
}

fn render(tree: &usvg::Tree, px: u32, path: &Path) -> Result<(), String> {
    let mut pixmap = tiny_skia::Pixmap::new(px, px).ok_or("empty size")?;
    let scale = px as f32 / tree.size().width();
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
        .save_png(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}
