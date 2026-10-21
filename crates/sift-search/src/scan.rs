//! Candidate generation and ranking over a [`Snapshot`].
//!
//! A query runs in up to two parallel passes over fixed-size chunks of the
//! snapshot:
//!
//! 1. **Substring pass.** One SIMD substring search (`memchr::memmem`) runs
//!    over each chunk's contiguous, NUL-separated name buffer. Every hit is
//!    mapped back to its name by advancing a cursor through the offsets
//!    array and graded as exact, exact-stem, prefix, word-prefix or plain
//!    substring.
//! 2. **Fuzzy pass.** Only when the substring pass found fewer matches than
//!    the result limit: fuzzy matches always rank below substring matches,
//!    so otherwise they could never appear. Names are rejected cheaply by
//!    character bag and an in-order byte check before `nucleo-matcher`
//!    scores the few survivors.
//!
//! Each chunk keeps a bounded top-k heap, so ranking work and memory are
//! proportional to the result limit, not the number of matches.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Range;

use memchr::memmem;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rayon::prelude::*;
use sift_store::EntryKind;

use crate::fold::{char_bag, fold};
use crate::snapshot::{Snapshot, depth_of, is_noisy, kind_of};
use crate::{KindFilter, MatchTier, Query};

/// Names per parallel work unit. Large enough to amortise task overhead,
/// small enough to spread a 100k-entry index across cores.
const CHUNK: usize = 16 * 1024;

thread_local! {
    // A matcher owns sizeable scratch buffers; keep one per worker thread
    // instead of allocating per query or per chunk.
    static MATCHER: RefCell<Matcher> = RefCell::new(Matcher::new(Config::DEFAULT));
}

/// A query folded and compiled for scanning.
pub(crate) struct Prepared {
    needle: Vec<u8>,
    finder: memmem::Finder<'static>,
    /// Characters every candidate must contain (whitespace excluded).
    bag: u64,
    multi_word: bool,
    fuzzy: Option<Pattern>,
    /// `.ext` suffixes, folded; a name must end in one of them.
    ext_suffixes: Vec<Vec<u8>>,
    kind: Option<KindFilter>,
    limit: usize,
}

impl Prepared {
    /// Returns `None` for queries that cannot match anything (empty text or
    /// a zero limit).
    pub(crate) fn new(query: &Query) -> Option<Self> {
        let needle = fold(query.text.trim().as_bytes());
        // A NUL would match the separators between names.
        if needle.is_empty() || needle.contains(&0) || query.limit == 0 {
            return None;
        }
        let multi_word = needle.iter().any(u8::is_ascii_whitespace);
        let bag = if multi_word {
            let compact: Vec<u8> = needle
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            char_bag(&compact)
        } else {
            char_bag(&needle)
        };
        let fuzzy = query.fuzzy.then(|| {
            let text = String::from_utf8_lossy(&needle);
            Pattern::new(
                &text,
                CaseMatching::Ignore,
                Normalization::Never,
                AtomKind::Fuzzy,
            )
        });
        let ext_suffixes = query
            .extensions
            .iter()
            .map(|ext| ext.trim_start_matches('.'))
            .filter(|ext| !ext.is_empty())
            .map(|ext| {
                let mut suffix = vec![b'.'];
                suffix.extend(fold(ext.as_bytes()));
                suffix
            })
            .collect();
        Some(Self {
            finder: memmem::Finder::new(&needle).into_owned(),
            needle,
            bag,
            multi_word,
            fuzzy,
            ext_suffixes,
            kind: query.kind,
            limit: query.limit,
        })
    }

    fn passes_filters(&self, meta: u16, name: &[u8]) -> bool {
        if let Some(kind) = self.kind {
            let is_dir = kind_of(meta) == EntryKind::Dir;
            if is_dir != (kind == KindFilter::Dirs) {
                return false;
            }
        }
        self.ext_suffixes.is_empty()
            || self
                .ext_suffixes
                .iter()
                .any(|suffix| name.ends_with(suffix) && name.len() > suffix.len())
    }
}

/// Scores individual folded names against a query with exactly the same
/// tiers and signals as an index scan. For small in-memory lists such as
/// installed applications, where a snapshot would be overkill.
pub struct NameMatcher {
    prepared: Prepared,
}

impl NameMatcher {
    /// Returns `None` when the query is blank.
    pub fn new(text: &str) -> Option<Self> {
        let query = Query::new(text);
        Prepared::new(&query).map(|prepared| Self { prepared })
    }

    /// Ranks `folded` (a name passed through [`crate::fold`]), returning its
    /// tier and sort key (higher is better), or `None` if it does not match.
    pub fn score(&self, folded: &[u8]) -> Option<(MatchTier, u64)> {
        let prepared = &self.prepared;
        let best = prepared
            .finder
            .find_iter(folded)
            .map(|at| substring_key(prepared, folded, at, 0, true))
            .max();
        if let Some(key) = best {
            return Some((tier_of(key), key));
        }
        let pattern = prepared.fuzzy.as_ref()?;
        if char_bag(folded) & prepared.bag != prepared.bag
            || (!prepared.multi_word && !is_subsequence(&prepared.needle, folded))
        {
            return None;
        }
        let text = std::str::from_utf8(folded).ok()?;
        let mut chars = Vec::new();
        let score =
            MATCHER.with_borrow_mut(|m| pattern.score(Utf32Str::new(text, &mut chars), m))?;
        let signals = Signals {
            clean: true,
            whole_word: false,
        };
        let key = rank_key(
            MatchTier::Fuzzy,
            signals,
            u16::try_from(score).unwrap_or(u16::MAX),
            folded.len(),
            0,
        );
        Some((MatchTier::Fuzzy, key))
    }
}

/// One ranked candidate: snapshot position plus its sort key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Ranked {
    pub key: u64,
    /// Ties go to the lower snapshot position (older entry).
    pub pos: Reverse<u32>,
}

/// Secondary ranking signals that decide between matches of the same tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Signals {
    /// Not inside a dependency, build or cache tree (see
    /// `snapshot::is_noise_dir`).
    pub clean: bool,
    /// The match ends where a word ends: `project` in `project-story`, not
    /// in `projection`.
    pub whole_word: bool,
}

/// Packs a ranking into one integer so comparisons are a single `cmp`.
///
/// | bits  | meaning |
/// |-------|---------|
/// | 48–55 | tier |
/// | 47    | clean (not under a dependency/build/cache folder) |
/// | 46    | whole-word match |
/// | 32–45 | tier-specific score: earlier match position, or fuzzy score |
/// | 16–31 | shorter name |
/// | 0–15  | shallower path |
pub(crate) fn rank_key(
    tier: MatchTier,
    signals: Signals,
    score: u16,
    name_len: usize,
    depth: u16,
) -> u64 {
    let len = u16::try_from(name_len).unwrap_or(u16::MAX);
    ((tier as u64) << 48)
        | ((signals.clean as u64) << 47)
        | ((signals.whole_word as u64) << 46)
        | ((score.min(SCORE_MASK) as u64) << 32)
        | (((u16::MAX - len) as u64) << 16)
        | ((u16::MAX - depth) as u64)
}

const SCORE_MASK: u16 = 0x3FFF;

pub(crate) fn tier_of(key: u64) -> MatchTier {
    MatchTier::from_u8((key >> 48) as u8)
}

/// Bounded min-heap that keeps the `limit` best candidates.
pub(crate) struct TopK {
    limit: usize,
    heap: BinaryHeap<Reverse<Ranked>>,
}

impl TopK {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::with_capacity(limit.min(1024) + 1),
        }
    }

    fn offer(&mut self, key: u64, pos: usize) {
        let item = Ranked {
            key,
            pos: Reverse(pos as u32),
        };
        if self.heap.len() < self.limit {
            self.heap.push(Reverse(item));
        } else if let Some(Reverse(worst)) = self.heap.peek()
            && item > *worst
        {
            self.heap.pop();
            self.heap.push(Reverse(item));
        }
    }

    fn merge(self, other: Self) -> Self {
        let (mut big, small) = if self.heap.len() >= other.heap.len() {
            (self, other)
        } else {
            (other, self)
        };
        for Reverse(item) in small.heap {
            big.offer(item.key, item.pos.0 as usize);
        }
        big
    }

    /// Best first.
    pub(crate) fn into_sorted(self) -> Vec<Ranked> {
        let mut items: Vec<Ranked> = self.heap.into_iter().map(|Reverse(r)| r).collect();
        items.sort_unstable_by(|a, b| b.cmp(a));
        items
    }
}

pub(crate) struct ScanResult {
    pub top: TopK,
    /// Substring matches (exact count) plus fuzzy matches (counted only
    /// when the fuzzy pass ran).
    pub matched: usize,
    pub fuzzy_ran: bool,
}

pub(crate) fn scan(snapshot: &Snapshot, prepared: &Prepared) -> ScanResult {
    let chunks: Vec<Range<usize>> = (0..snapshot.len())
        .step_by(CHUNK)
        .map(|start| start..(start + CHUNK).min(snapshot.len()))
        .collect();

    let substring = chunks
        .par_iter()
        .map(|range| substring_chunk(snapshot, prepared, range.clone()))
        .reduce(|| ChunkHits::new(prepared.limit), ChunkHits::merge);

    let Some(pattern) = &prepared.fuzzy else {
        return ScanResult {
            top: substring.top,
            matched: substring.count,
            fuzzy_ran: false,
        };
    };
    if substring.count >= prepared.limit {
        return ScanResult {
            top: substring.top,
            matched: substring.count,
            fuzzy_ran: false,
        };
    }

    // Fewer substring matches than the limit, so `positions` is complete and
    // small; the fuzzy pass uses it to skip names already ranked.
    let mut already = substring.positions;
    already.sort_unstable();
    let fuzzy = chunks
        .par_iter()
        .map(|range| fuzzy_chunk(snapshot, prepared, pattern, &already, range.clone()))
        .reduce(|| ChunkHits::new(prepared.limit), ChunkHits::merge);

    ScanResult {
        top: substring.top.merge(fuzzy.top),
        matched: substring.count + fuzzy.count,
        fuzzy_ran: true,
    }
}

struct ChunkHits {
    top: TopK,
    count: usize,
    /// Positions of matched names, kept only while `count <= limit`.
    positions: Vec<u32>,
}

impl ChunkHits {
    fn new(limit: usize) -> Self {
        Self {
            top: TopK::new(limit),
            count: 0,
            positions: Vec::new(),
        }
    }

    fn record(&mut self, key: u64, pos: usize) {
        self.count += 1;
        if self.count <= self.top.limit {
            self.positions.push(pos as u32);
        }
        self.top.offer(key, pos);
    }

    fn merge(mut self, other: Self) -> Self {
        self.count += other.count;
        if self.count <= self.top.limit {
            self.positions.extend(other.positions);
        } else {
            self.positions = Vec::new();
        }
        self.top = self.top.merge(other.top);
        self
    }
}

fn substring_chunk(snapshot: &Snapshot, prepared: &Prepared, range: Range<usize>) -> ChunkHits {
    let mut hits = ChunkHits::new(prepared.limit);
    let offsets = snapshot.offsets();
    let meta = snapshot.meta();
    let base = offsets[range.start] as usize;
    let blob = &snapshot.names_blob()[base..offsets[range.end] as usize];

    // Best key for the name currently under the cursor; a name can contain
    // the needle several times and only its best occurrence counts.
    let mut pending: Option<(usize, u64)> = None;
    let mut pos = range.start;
    for hit in prepared.finder.find_iter(blob) {
        let at = base + hit;
        while offsets[pos + 1] as usize <= at {
            pos += 1;
        }
        let start = offsets[pos] as usize;
        let name = snapshot.name(pos);
        if !prepared.passes_filters(meta[pos], name) {
            continue;
        }
        let key = substring_key(
            prepared,
            name,
            at - start,
            depth_of(meta[pos]),
            !is_noisy(meta[pos]),
        );
        match &mut pending {
            Some((p, best)) if *p == pos => *best = (*best).max(key),
            _ => {
                if let Some((p, best)) = pending.take() {
                    hits.record(best, p);
                }
                pending = Some((pos, key));
            }
        }
    }
    if let Some((p, best)) = pending {
        hits.record(best, p);
    }
    hits
}

fn substring_key(prepared: &Prepared, name: &[u8], at: usize, depth: u16, clean: bool) -> u64 {
    let needle_len = prepared.needle.len();
    let tier = if at == 0 && name.len() == needle_len {
        MatchTier::Exact
    } else if at == 0 && is_stem(name, needle_len) {
        MatchTier::ExactStem
    } else if at == 0 {
        MatchTier::Prefix
    } else if is_word_start(name, at) {
        MatchTier::WordPrefix
    } else {
        MatchTier::Substring
    };
    let signals = Signals {
        clean,
        whole_word: ends_word(name, at + needle_len),
    };
    let at = u16::try_from(at).unwrap_or(SCORE_MASK).min(SCORE_MASK);
    rank_key(tier, signals, SCORE_MASK - at, name.len(), depth)
}

/// Whether a match ending at byte `end` finishes a word: the name ends there
/// or the next character is punctuation or a space. Non-ASCII bytes count as
/// letters.
fn ends_word(name: &[u8], end: usize) -> bool {
    match name.get(end) {
        None => true,
        Some(&b) => b < 0x80 && !b.is_ascii_alphanumeric(),
    }
}

/// Whether `name[..len]` is the name without its final extension, as in
/// `report` for `report.pdf`.
fn is_stem(name: &[u8], len: usize) -> bool {
    name.get(len) == Some(&b'.') && !name[len + 1..].contains(&b'.') && len + 1 < name.len()
}

fn is_word_start(name: &[u8], at: usize) -> bool {
    matches!(
        name[at - 1],
        b' ' | b'-' | b'_' | b'.' | b'(' | b'[' | b'{' | b'+' | b','
    ) || (name[at - 1].is_ascii_alphabetic() && name[at].is_ascii_digit())
}

fn fuzzy_chunk(
    snapshot: &Snapshot,
    prepared: &Prepared,
    pattern: &Pattern,
    already: &[u32],
    range: Range<usize>,
) -> ChunkHits {
    let mut hits = ChunkHits::new(prepared.limit);
    let bags = snapshot.bags();
    let meta = snapshot.meta();
    let mut chars = Vec::new();

    MATCHER.with_borrow_mut(|matcher| {
        for pos in range {
            if bags[pos] & prepared.bag != prepared.bag {
                continue;
            }
            let name = snapshot.name(pos);
            if !prepared.multi_word && !is_subsequence(&prepared.needle, name) {
                continue;
            }
            if !prepared.passes_filters(meta[pos], name)
                || already.binary_search(&(pos as u32)).is_ok()
            {
                continue;
            }
            let Ok(text) = std::str::from_utf8(name) else {
                continue;
            };
            let Some(score) = pattern.score(Utf32Str::new(text, &mut chars), matcher) else {
                continue;
            };
            let signals = Signals {
                clean: !is_noisy(meta[pos]),
                whole_word: false,
            };
            let score = u16::try_from(score).unwrap_or(u16::MAX);
            let key = rank_key(
                MatchTier::Fuzzy,
                signals,
                score,
                name.len(),
                depth_of(meta[pos]),
            );
            hits.record(key, pos);
        }
    });
    hits
}

/// Whether the bytes of `needle` occur in `haystack` in order. Necessary
/// (but not sufficient) for a fuzzy match, and much cheaper to check.
fn is_subsequence(needle: &[u8], haystack: &[u8]) -> bool {
    let mut rest = haystack;
    for &b in needle {
        match memchr::memchr(b, rest) {
            Some(i) => rest = &rest[i + 1..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Signals = Signals {
        clean: true,
        whole_word: false,
    };

    #[test]
    fn rank_keys_order_by_tier_then_score_then_length_then_depth() {
        let exact = rank_key(MatchTier::Exact, PLAIN, 0, 50, 9);
        let prefix = rank_key(MatchTier::Prefix, PLAIN, u16::MAX, 1, 0);
        assert!(exact > prefix);
        assert!(
            rank_key(MatchTier::Fuzzy, PLAIN, 10, 5, 0)
                > rank_key(MatchTier::Fuzzy, PLAIN, 9, 1, 0)
        );
        assert!(
            rank_key(MatchTier::Prefix, PLAIN, 5, 4, 9)
                > rank_key(MatchTier::Prefix, PLAIN, 5, 5, 0)
        );
        assert!(
            rank_key(MatchTier::Prefix, PLAIN, 5, 4, 1)
                > rank_key(MatchTier::Prefix, PLAIN, 5, 4, 2)
        );
        assert_eq!(tier_of(prefix), MatchTier::Prefix);
        // Oversized fuzzy scores saturate instead of spilling into the flags.
        let huge = rank_key(MatchTier::Fuzzy, PLAIN, u16::MAX, 1, 0);
        assert_eq!(huge >> 46 & 0b11, 0b10);
    }

    #[test]
    fn clean_beats_whole_word_beats_score_within_a_tier() {
        let noisy = Signals {
            clean: false,
            whole_word: true,
        };
        let word = Signals {
            clean: true,
            whole_word: true,
        };
        let clean = rank_key(MatchTier::Prefix, PLAIN, 0, 99, 9);
        assert!(clean > rank_key(MatchTier::Prefix, noisy, SCORE_MASK, 1, 0));
        assert!(
            rank_key(MatchTier::Prefix, word, 0, 99, 9)
                > rank_key(MatchTier::Prefix, PLAIN, SCORE_MASK, 1, 0)
        );
        // But never across tiers.
        assert!(
            rank_key(MatchTier::Prefix, PLAIN, 0, 99, 9)
                > rank_key(MatchTier::WordPrefix, word, SCORE_MASK, 1, 0)
        );
    }

    #[test]
    fn word_endings() {
        assert!(ends_word(b"project-story", 7));
        assert!(ends_word(b"project", 7));
        assert!(!ends_word(b"projection", 7));
        assert!(!ends_word("projecté".as_bytes(), 7));
    }

    #[test]
    fn name_matcher_uses_index_ranking() {
        let m = NameMatcher::new("Safari").unwrap();
        assert_eq!(m.score(b"safari").unwrap().0, MatchTier::Exact);
        let code = NameMatcher::new("vsc").unwrap();
        assert_eq!(
            code.score(b"visual studio code").unwrap().0,
            MatchTier::Fuzzy
        );
        assert!(code.score(b"calendar").is_none());
        let term = NameMatcher::new("term").unwrap();
        let (_, terminal) = term.score(b"terminal").unwrap();
        let (_, iterm) = term.score(b"iterm").unwrap();
        assert!(terminal > iterm);
        assert!(NameMatcher::new("  ").is_none());
    }

    #[test]
    fn top_k_keeps_best_and_merges() {
        let mut a = TopK::new(3);
        for (key, pos) in [(5, 0), (1, 1), (9, 2), (7, 3)] {
            a.offer(key, pos);
        }
        let mut b = TopK::new(3);
        b.offer(8, 4);
        b.offer(9, 5);
        let keys: Vec<_> = a.merge(b).into_sorted().iter().map(|r| r.key).collect();
        assert_eq!(keys, [9, 9, 8]);
    }

    #[test]
    fn stem_and_word_detection() {
        assert!(is_stem(b"report.pdf", 6));
        assert!(!is_stem(b"report.tar.gz", 6));
        assert!(!is_stem(b"report.", 6));
        assert!(is_word_start(b"my-report", 3));
        assert!(is_word_start(b"v2", 1));
        assert!(!is_word_start(b"export", 2));
    }

    #[test]
    fn subsequence() {
        assert!(is_subsequence(b"prs", b"project.rs"));
        assert!(!is_subsequence(b"psr", b"project.s"));
    }
}
