use std::fs;
use std::os::unix::fs::symlink;

use super::*;

/// Creates a fake bundle with an XML Info.plist (or `plist` verbatim).
fn bundle(dir: &Path, name: &str, plist: Option<&str>) -> PathBuf {
    let path = dir.join(format!("{name}.app"));
    fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
    if let Some(body) = plist {
        fs::write(path.join("Contents/Info.plist"), body).unwrap();
    }
    path
}

fn info(id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>{id}</string></dict></plist>"#
    )
}

fn names(catalog: &AppCatalog) -> Vec<String> {
    catalog.apps().iter().map(|a| a.name.to_string()).collect()
}

fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    (dir, base)
}

#[test]
fn discovers_bundles_in_folders_and_one_level_down() {
    let (_t, base) = temp();
    let apps = base.join("Applications");
    bundle(&apps, "Safari", Some(&info("com.apple.Safari")));
    bundle(
        &apps.join("Utilities"),
        "Terminal",
        Some(&info("com.apple.Terminal")),
    );
    bundle(&apps.join("Vendor/Deep"), "TooDeep", None);
    // Helper apps inside a bundle are not separate applications.
    bundle(&apps.join("Safari.app/Contents/Helpers"), "Helper", None);
    fs::write(apps.join("notes.txt"), b"x").unwrap();
    bundle(&apps.join(".hidden"), "Secret", None);

    let catalog = AppCatalog::discover(&[Location::Folder(apps.clone())]);
    assert_eq!(names(&catalog), ["Safari", "Terminal"]);
    let terminal = &catalog.apps()[1];
    assert_eq!(terminal.bundle_id.as_deref(), Some("com.apple.Terminal"));
    assert_eq!(
        terminal.location.as_ref(),
        apps.join("Utilities").to_str().unwrap()
    );
}

#[test]
fn deduplicates_symlinks_and_bundle_ids() {
    let (_t, base) = temp();
    let system = base.join("System/Applications");
    let apps = base.join("Applications");
    let user = base.join("User/Applications");
    let real = bundle(&system, "Safari", Some(&info("com.apple.Safari")));
    fs::create_dir_all(&apps).unwrap();
    symlink(&real, apps.join("Safari.app")).unwrap();
    bundle(&apps, "Editor", Some(&info("dev.editor")));
    bundle(&user, "Editor Copy", Some(&info("dev.editor")));

    let catalog = AppCatalog::discover(&[
        Location::Folder(apps.clone()),
        Location::Folder(user),
        Location::Folder(system),
    ]);
    assert_eq!(names(&catalog), ["Editor", "Safari"]);
    // The first location wins, and the symlinked path is what is kept.
    let safari = catalog
        .apps()
        .iter()
        .find(|a| &*a.name == "Safari")
        .unwrap();
    assert_eq!(safari.path, apps.join("Safari.app"));
}

#[test]
fn tolerates_missing_locations_and_malformed_bundles() {
    let (_t, base) = temp();
    let apps = base.join("Applications");
    bundle(&apps, "NoPlist", None);
    bundle(&apps, "Garbage", Some("this is not a plist"));
    bundle(&apps, "Weird Name (Beta) ✨", Some(&info("dev.weird")));
    symlink(base.join("nowhere"), apps.join("Broken.app")).unwrap();

    let catalog = AppCatalog::discover(&[
        Location::Folder(base.join("missing")),
        Location::Bundle(base.join("Missing.app")),
        Location::Folder(apps),
    ]);
    assert_eq!(
        names(&catalog),
        ["Garbage", "NoPlist", "Weird Name (Beta) ✨"]
    );
    assert!(catalog.apps().iter().take(2).all(|a| a.bundle_id.is_none()));
}

#[test]
fn single_bundle_locations() {
    let (_t, base) = temp();
    let finder = bundle(
        &base.join("CoreServices"),
        "Finder",
        Some(&info("com.apple.finder")),
    );
    let catalog = AppCatalog::discover(&[Location::Bundle(finder.clone())]);
    assert_eq!(names(&catalog), ["Finder"]);
    assert_eq!(catalog.apps()[0].path, finder);
}

fn provider(names: &[&str]) -> (tempfile::TempDir, AppProvider) {
    let (t, base) = temp();
    let apps = base.join("Applications");
    for (i, name) in names.iter().enumerate() {
        bundle(&apps, name, Some(&info(&format!("app.{i}"))));
    }
    let catalog = AppCatalog::discover(&[Location::Folder(apps)]);
    (t, AppProvider::new(catalog))
}

fn titles(p: &AppProvider, query: &str) -> Vec<String> {
    p.search(query, 10)
        .into_iter()
        .map(|r| r.title.to_string())
        .collect()
}

#[test]
fn ranks_exact_then_prefix_then_word_then_fuzzy() {
    let (_t, p) = provider(&[
        "Calendar",
        "Calculator",
        "Visual Studio Code",
        "Xcode",
        "iTerm",
        "Terminal",
        "Safari",
        "Safari Technology Preview",
    ]);
    assert_eq!(
        titles(&p, "safari"),
        ["Safari", "Safari Technology Preview"]
    );
    assert_eq!(titles(&p, "cal"), ["Calendar", "Calculator"]);
    assert_eq!(titles(&p, "term"), ["Terminal", "iTerm"]);
    // Word prefix ("Code" in "Visual Studio Code") beats a substring hit.
    assert_eq!(titles(&p, "code"), ["Visual Studio Code", "Xcode"]);
    // Fuzzy initials.
    assert_eq!(titles(&p, "vsc"), ["Visual Studio Code"]);
    assert_eq!(titles(&p, "SAFARI TECH"), ["Safari Technology Preview"]);
}

#[test]
fn result_items_describe_the_app() {
    let (_t, p) = provider(&["Notes"]);
    let results = p.search("notes", 5);
    assert_eq!(results.len(), 1);
    let item = &results[0];
    assert_eq!(item.kind, ResultKind::Application);
    let Action::OpenApplication(path) = &item.action else {
        panic!("wrong action");
    };
    assert!(path.ends_with("Applications/Notes.app"));
    assert_eq!(item.id.as_ref(), path.to_str().unwrap());
    assert!(item.subtitle.as_deref().unwrap().ends_with("Applications"));
    assert_eq!(p.name(), "apps");
}

#[test]
fn blank_queries_and_limits() {
    let (_t, p) = provider(&["Alpha", "Alphabet", "Alpine"]);
    assert!(p.search("", 10).is_empty());
    assert!(p.search("   ", 10).is_empty());
    assert_eq!(p.search("al", 2).len(), 2);
    assert!(p.search("zzz", 10).is_empty());
}

/// Discovery on this machine's real application folders: checks it runs,
/// finds the always-present system apps, and has no duplicates.
#[test]
fn discovers_real_system_apps() {
    if !Path::new("/System/Applications").exists() {
        return;
    }
    let catalog = AppCatalog::discover(&Location::defaults());
    let names = names(&catalog);
    for expected in ["Finder", "Calculator", "System Settings"] {
        assert!(names.iter().any(|n| n == expected), "missing {expected}");
    }
    let mut paths: Vec<_> = catalog
        .apps()
        .iter()
        .map(|a| a.path.canonicalize().unwrap())
        .collect();
    let total = paths.len();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), total, "duplicate bundles");
}
