//! Deterministic synthetic file trees for benchmarks.
//!
//! The same seed and count always produce the same tree, so results are
//! reproducible across runs and machines. Names mix common words,
//! separators, numbers and extensions in proportions loosely modelled on a
//! documents-and-code home directory.

use std::collections::HashSet;
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};

const WORDS: &[&str] = &[
    "project",
    "report",
    "invoice",
    "notes",
    "draft",
    "final",
    "budget",
    "meeting",
    "summary",
    "photo",
    "image",
    "screenshot",
    "scan",
    "receipt",
    "contract",
    "letter",
    "resume",
    "slides",
    "presentation",
    "assignment",
    "homework",
    "lecture",
    "thesis",
    "chapter",
    "paper",
    "review",
    "backup",
    "archive",
    "export",
    "import",
    "data",
    "dataset",
    "results",
    "analysis",
    "plan",
    "roadmap",
    "design",
    "mockup",
    "logo",
    "icon",
    "assets",
    "src",
    "main",
    "lib",
    "test",
    "tests",
    "utils",
    "config",
    "settings",
    "readme",
    "license",
    "changelog",
    "index",
    "app",
    "server",
    "client",
    "api",
    "model",
    "view",
    "controller",
    "schema",
    "migration",
    "build",
    "release",
    "debug",
    "docs",
    "guide",
    "manual",
    "tutorial",
    "example",
    "sample",
    "template",
    "old",
    "new",
    "copy",
    "temp",
    "music",
    "video",
    "travel",
    "family",
    "work",
    "personal",
    "tax",
    "bank",
    "health",
    "recipe",
    "annual",
    "quarterly",
    "weekly",
    "monday",
    "january",
    "q3",
    "v2",
    "alpha",
];

const EXTENSIONS: &[&str] = &[
    "txt", "md", "pdf", "pdf", "jpg", "jpg", "png", "png", "rs", "js", "ts", "json", "docx",
    "xlsx", "pptx", "swift", "py", "html", "css", "zip", "mov", "mp3", "csv", "toml", "yaml", "sh",
    "c", "h", "key", "pages",
];

/// xorshift64* — small, fast and stable across platforms and releases,
/// unlike the standard library's randomly seeded hashers.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// One generated entry. `parent` is the index of an earlier directory
/// entry, or `None` for children of the root.
pub struct SynthEntry<'a> {
    pub index: usize,
    pub parent: Option<usize>,
    pub is_dir: bool,
    pub name: &'a str,
}

/// Generates `count` entries (excluding the root), calling `emit` for each
/// in an order where parents always precede their children.
pub fn generate(count: usize, seed: u64, mut emit: impl FnMut(SynthEntry<'_>)) {
    let mut rng = Rng::new(seed);
    // (index, depth) of every directory so far.
    let mut dirs: Vec<(usize, u32)> = Vec::new();
    // Hashes of (parent, lowercased name) pairs, to keep names unique per
    // directory without storing every name. Lowercased because APFS is
    // case-insensitive by default. Fixed hasher keys keep output stable.
    let hasher = BuildHasherDefault::<DefaultHasher>::default();
    let mut taken: HashSet<u64> = HashSet::with_capacity(count);
    let mut name = String::new();

    for index in 0..count {
        let is_dir = dirs.is_empty() || rng.below(16) == 0;
        // Mostly attach to a recent directory, giving clustered, realistic
        // depth; occasionally to the root or any earlier directory.
        let parent = match rng.below(20) {
            0 => None,
            1 if !dirs.is_empty() => Some(dirs[rng.below(dirs.len())]),
            _ if dirs.is_empty() => None,
            _ => {
                let recent = dirs.len().min(48);
                Some(dirs[dirs.len() - 1 - rng.below(recent)])
            }
        };
        // Keep the tree from growing unrealistically deep.
        let parent = parent.filter(|&(_, depth)| depth < 10);

        name.clear();
        if is_dir {
            dir_name(&mut rng, &mut name);
        } else {
            file_name(&mut rng, &mut name);
        }
        let parent_index = parent.map(|(i, _)| i);
        // On a collision within the directory, disambiguate like Finder.
        let base_len = name.len();
        let mut copy = 1;
        while !taken.insert(hasher.hash_one((parent_index, name.to_ascii_lowercase()))) {
            copy += 1;
            name.truncate(base_len);
            name.push_str(&format!(" {copy}"));
        }

        emit(SynthEntry {
            index,
            parent: parent_index,
            is_dir,
            name: &name,
        });
        if is_dir {
            let depth = parent.map_or(1, |(_, d)| d + 1);
            dirs.push((index, depth));
        }
    }
}

fn capitalize(word: &str, out: &mut String) {
    let mut chars = word.chars();
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
}

fn dir_name(rng: &mut Rng, out: &mut String) {
    match rng.below(5) {
        0 | 1 => out.push_str(rng.pick(WORDS)),
        2 => {
            out.push_str(rng.pick(WORDS));
            out.push('-');
            out.push_str(rng.pick(WORDS));
        }
        3 => {
            capitalize(rng.pick(WORDS), out);
            out.push(' ');
            out.push_str(&(2015 + rng.below(12)).to_string());
        }
        _ => capitalize(rng.pick(WORDS), out),
    }
}

fn file_name(rng: &mut Rng, out: &mut String) {
    match rng.below(8) {
        0 | 1 => {
            out.push_str(rng.pick(WORDS));
            out.push('-');
            out.push_str(rng.pick(WORDS));
            out.push('-');
            out.push_str(&rng.below(1000).to_string());
        }
        2 => {
            capitalize(rng.pick(WORDS), out);
            out.push(' ');
            capitalize(rng.pick(WORDS), out);
            out.push(' ');
            out.push_str(&(2015 + rng.below(12)).to_string());
        }
        3 => {
            out.push_str(rng.pick(WORDS));
            out.push('_');
            out.push_str(rng.pick(WORDS));
        }
        4 => {
            out.push_str(["IMG_", "DSC_", "Screenshot "][rng.below(3)]);
            out.push_str(&format!("{:04}", rng.below(10_000)));
        }
        5 => {
            out.push_str(rng.pick(WORDS));
            out.push_str(&rng.below(100).to_string());
        }
        _ => out.push_str(rng.pick(WORDS)),
    }
    out.push('.');
    out.push_str(rng.pick(EXTENSIONS));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(count: usize, seed: u64) -> Vec<(Option<usize>, bool, String)> {
        let mut out = Vec::new();
        generate(count, seed, |e| {
            out.push((e.parent, e.is_dir, e.name.to_string()))
        });
        out
    }

    #[test]
    fn deterministic_for_a_seed() {
        assert_eq!(collect(500, 7), collect(500, 7));
        assert_ne!(collect(500, 7), collect(500, 8));
    }

    #[test]
    fn parents_are_earlier_directories_and_names_unique_per_dir() {
        let entries = collect(5000, 42);
        let mut seen = HashSet::new();
        for (i, (parent, _, name)) in entries.iter().enumerate() {
            if let Some(p) = parent {
                assert!(*p < i);
                assert!(entries[*p].1, "parent must be a directory");
            }
            let key = (*parent, name.to_ascii_lowercase());
            assert!(seen.insert(key), "duplicate {name}");
        }
        let dirs = entries.iter().filter(|e| e.1).count();
        assert!(dirs > 100 && dirs < 1000, "{dirs} dirs");
    }
}
