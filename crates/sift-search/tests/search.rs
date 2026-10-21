use std::fs;
use std::path::{Path, PathBuf};

use sift_indexer::{IndexOptions, index_root};
use sift_search::{DB_FILE, KindFilter, MatchTier, Query, SNAPSHOT_FILE, SearchIndex};
use sift_store::{EntryKind, NewEntry, ScanSummary, Store};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    index_dir: PathBuf,
}

impl Fixture {
    fn new(files: &[&str]) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let root = base.join("root");
        let index_dir = base.join("index");
        fs::create_dir_all(&root).expect("create root");
        fs::create_dir_all(&index_dir).expect("create index dir");
        for file in files {
            let path = root.join(file);
            if file.ends_with('/') {
                fs::create_dir_all(&path).expect("create dir");
            } else {
                fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
                fs::write(&path, b"x").expect("write");
            }
        }
        let fixture = Self {
            _tmp: tmp,
            root,
            index_dir,
        };
        fixture.reindex();
        fixture
    }

    fn reindex(&self) {
        let mut store = Store::open(&self.index_dir.join(DB_FILE)).expect("open store");
        index_root(&mut store, &self.root, &IndexOptions::default(), |_| {}).expect("index");
    }

    fn open(&self) -> SearchIndex {
        SearchIndex::open(&self.index_dir).expect("open index")
    }

    /// Paths of the results relative to the root, best first.
    fn search(&self, query: &Query) -> Vec<(String, MatchTier)> {
        let index = self.open();
        let outcome = index.search(query);
        index
            .resolve(&outcome.hits)
            .expect("resolve")
            .into_iter()
            .map(|r| {
                let rel = r.path.strip_prefix(&self.root).expect("under root");
                (rel.to_string_lossy().into_owned(), r.tier)
            })
            .collect()
    }

    fn names(&self, text: &str) -> Vec<String> {
        self.search(&Query::new(text))
            .into_iter()
            .map(|(p, _)| p)
            .collect()
    }
}

#[test]
fn ranks_exact_above_prefix_above_substring_above_fuzzy() {
    let f = Fixture::new(&[
        "a/p_r_o_j_e_c_t.txt",
        "a/subproject.txt",
        "a/my-project-notes.md",
        "a/projects.txt",
        "a/project.rs",
        "a/project/",
        "a/unrelated.txt",
    ]);
    let results = f.search(&Query::new("project"));
    let expected = [
        ("a/project", MatchTier::Exact),
        ("a/project.rs", MatchTier::ExactStem),
        ("a/projects.txt", MatchTier::Prefix),
        ("a/my-project-notes.md", MatchTier::WordPrefix),
        ("a/subproject.txt", MatchTier::Substring),
        ("a/p_r_o_j_e_c_t.txt", MatchTier::Fuzzy),
    ];
    let expected: Vec<_> = expected.iter().map(|(p, t)| (p.to_string(), *t)).collect();
    assert_eq!(results, expected);
}

#[test]
fn shorter_names_then_shallower_paths_win_within_a_tier() {
    let f = Fixture::new(&["deep/er/notes-long.txt", "notes-longer.txt", "x/notes.txt"]);
    assert_eq!(
        f.names("notes"),
        ["x/notes.txt", "deep/er/notes-long.txt", "notes-longer.txt"]
    );

    let f = Fixture::new(&["a/b/c/main.rs", "main.rs", "a/main.rs"]);
    assert_eq!(
        f.names("main.rs"),
        ["main.rs", "a/main.rs", "a/b/c/main.rs"]
    );
}

#[test]
fn matching_ignores_case_accents_and_normalization_form() {
    let f = Fixture::new(&["Café Menu.PDF", "cafe\u{301}-decomposed.txt", "RÉSUMÉ.docx"]);
    let mut cafe = f.names("CAFE");
    cafe.sort();
    assert_eq!(cafe, ["Café Menu.PDF", "cafe\u{301}-decomposed.txt"]);
    assert_eq!(f.names("café menu"), ["Café Menu.PDF"]);
    assert_eq!(f.names("resume"), ["RÉSUMÉ.docx"]);
}

#[test]
fn finds_non_latin_and_unusual_names() {
    let f = Fixture::new(&[
        "日本語のファイル.md",
        "emoji 🚀 launch.png",
        "quote's \"double\".txt",
        "semi;colon&amp.txt",
    ]);
    assert_eq!(f.names("日本"), ["日本語のファイル.md"]);
    assert_eq!(f.names("🚀"), ["emoji 🚀 launch.png"]);
    assert_eq!(f.names("'s \"double"), ["quote's \"double\".txt"]);
    assert_eq!(f.names(";colon&"), ["semi;colon&amp.txt"]);
}

#[test]
fn fuzzy_matching() {
    let f = Fixture::new(&["src/project.rs", "Annual-Report-2024.pdf", "other.txt"]);
    assert_eq!(f.names("prjrs"), ["src/project.rs"]);
    // Words may match separately and out of order.
    assert_eq!(f.names("report annual"), ["Annual-Report-2024.pdf"]);

    let strict = Query {
        fuzzy: false,
        ..Query::new("prjrs")
    };
    assert!(f.search(&strict).is_empty());
}

#[test]
fn fuzzy_pass_is_skipped_when_substring_matches_fill_the_limit() {
    let f = Fixture::new(&["ab1", "ab2", "ab3", "a_b"]);
    let index = f.open();
    let outcome = index.search(&Query::new("ab").limit(3));
    assert_eq!(outcome.hits.len(), 3);
    assert!(!outcome.fuzzy_ran);
    assert!(outcome.hits.iter().all(|h| h.tier != MatchTier::Fuzzy));

    let outcome = index.search(&Query::new("ab").limit(10));
    assert!(outcome.fuzzy_ran);
    assert_eq!(outcome.hits.len(), 4);
}

#[test]
fn empty_and_blank_queries_return_nothing() {
    let f = Fixture::new(&["a.txt"]);
    let index = f.open();
    for text in ["", "   ", "\t"] {
        let outcome = index.search(&Query::new(text));
        assert!(outcome.hits.is_empty(), "{text:?}");
        assert_eq!(outcome.matched, 0);
    }
    assert!(index.search(&Query::new("a").limit(0)).hits.is_empty());
}

#[test]
fn respects_limit_and_counts_all_matches() {
    let files: Vec<String> = (0..50).map(|i| format!("file{i:02}.log")).collect();
    let refs: Vec<&str> = files.iter().map(String::as_str).collect();
    let f = Fixture::new(&refs);
    let index = f.open();
    let outcome = index.search(&Query::new("file").limit(7));
    assert_eq!(outcome.hits.len(), 7);
    assert_eq!(outcome.matched, 50);
}

#[test]
fn filters_by_kind_and_extension() {
    let f = Fixture::new(&[
        "report/",
        "report.pdf",
        "report.PDF.bak",
        "q3-report.PDF",
        "report.txt",
    ]);
    let dirs = Query {
        kind: Some(KindFilter::Dirs),
        ..Query::new("report")
    };
    assert_eq!(f.search(&dirs).len(), 1);

    let files = Query {
        kind: Some(KindFilter::Files),
        ..Query::new("report")
    };
    assert_eq!(f.search(&files).len(), 4);

    let pdf = Query {
        extension: Some(".pdf".into()),
        ..Query::new("report")
    };
    let mut names: Vec<_> = f.search(&pdf).into_iter().map(|(p, _)| p).collect();
    names.sort();
    assert_eq!(names, ["q3-report.PDF", "report.pdf"]);
}

#[test]
fn duplicate_names_in_different_directories_are_all_found() {
    let f = Fixture::new(&["one/Cargo.toml", "two/Cargo.toml", "two/three/Cargo.toml"]);
    let mut names = f.names("cargo.toml");
    // Equal-depth duplicates tie; their relative order follows entry ids,
    // which the parallel scanner allocates in no fixed order.
    assert_eq!(names.pop().as_deref(), Some("two/three/Cargo.toml"));
    names.sort();
    assert_eq!(names, ["one/Cargo.toml", "two/Cargo.toml"]);
}

#[test]
fn root_directory_is_searchable_by_its_name() {
    let f = Fixture::new(&["inner.txt"]);
    let index = f.open();
    let outcome = index.search(&Query::new("root"));
    let results = index.resolve(&outcome.hits).unwrap();
    assert_eq!(results[0].path, f.root);
    assert_eq!(results[0].kind, EntryKind::Dir);
}

#[test]
fn searches_persisted_index_without_touching_the_filesystem() {
    let f = Fixture::new(&["keep/needle.txt"]);
    drop(f.open()); // build the snapshot
    fs::remove_dir_all(&f.root).unwrap();
    // The index still answers from disk: no rescan happens on search.
    assert_eq!(f.names("needle"), ["keep/needle.txt"]);
}

#[test]
fn stale_snapshot_is_rebuilt_after_reindex() {
    let f = Fixture::new(&["before.txt"]);
    let first = f.open();
    assert!(first.snapshot_rebuilt());
    assert!(!f.open().snapshot_rebuilt());

    fs::remove_file(f.root.join("before.txt")).unwrap();
    fs::write(f.root.join("after.txt"), b"x").unwrap();
    f.reindex();

    let index = f.open();
    assert!(index.snapshot_rebuilt());
    assert!(index.search(&Query::new("before")).hits.is_empty());
    assert_eq!(index.search(&Query::new("after")).hits.len(), 1);
    // The old handle keeps working on its own mapping.
    assert_eq!(first.search(&Query::new("before")).hits.len(), 1);
}

#[test]
fn corrupt_or_truncated_snapshot_is_rebuilt() {
    let f = Fixture::new(&["target.txt"]);
    drop(f.open());
    let snap = f.index_dir.join(SNAPSHOT_FILE);
    let good = fs::read(&snap).unwrap();

    let mut flipped = good.clone();
    let last = flipped.len() - 1;
    flipped[last] = b'x'; // final name separator
    for bad in [
        b"garbage".to_vec(),
        good[..good.len() / 2].to_vec(),
        flipped,
    ] {
        fs::write(&snap, bad).unwrap();
        let index = f.open();
        assert!(index.snapshot_rebuilt());
        assert_eq!(index.search(&Query::new("target")).hits.len(), 1);
    }
}

#[test]
fn missing_or_corrupt_database_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(
        SearchIndex::open(tmp.path()),
        Err(sift_search::SearchError::NoIndex(_))
    ));
    fs::write(tmp.path().join(DB_FILE), vec![7u8; 4096]).unwrap();
    assert!(matches!(
        SearchIndex::open(tmp.path()),
        Err(sift_search::SearchError::Store(_))
    ));
}

/// Builds an index directly in the store (no filesystem), large enough to
/// span many scan chunks, and checks the parallel scan against brute force.
#[test]
fn large_index_matches_brute_force() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut store = Store::open(&dir.join(DB_FILE)).unwrap();
    let words = [
        "alpha", "beta", "gamma", "delta", "report", "draft", "final",
    ];
    let mut names = Vec::new();
    {
        let mut w = store.replace_root(Path::new("/synthetic"), 1).unwrap();
        for i in 0..60_000u32 {
            let name = format!(
                "{}-{}-{}.txt",
                words[(i % 7) as usize],
                words[((i / 7) % 7) as usize],
                i
            );
            w.insert(&NewEntry {
                id: 2 + i as i64,
                parent_id: 1,
                kind: EntryKind::File,
                name: name.as_bytes(),
                size: Some(1),
                mtime: None,
            })
            .unwrap();
            names.push(name);
        }
        w.commit(ScanSummary::default()).unwrap();
    }
    drop(store);

    let index = SearchIndex::open(dir).unwrap();
    assert_eq!(index.snapshot().len(), 60_001);
    for text in ["report-draft", "ta-g", "999", "final-final-5"] {
        let expected = names.iter().filter(|n| n.contains(text)).count();
        let outcome = index.search(&Query::new(text).limit(10));
        assert_eq!(outcome.matched, expected, "{text}");
        assert_eq!(outcome.hits.len(), expected.min(10), "{text}");
        let results = index.resolve(&outcome.hits).unwrap();
        for pair in outcome.hits.windows(2) {
            assert!(pair[0].score >= pair[1].score);
        }
        for result in results {
            let name = result.path.file_name().unwrap().to_string_lossy();
            assert!(name.contains(text), "{name} !~ {text}");
        }
    }
}
