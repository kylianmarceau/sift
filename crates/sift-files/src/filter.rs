//! Search filters typed into the query, Everything-style:
//!
//! | token                 | meaning                                         |
//! |-----------------------|-------------------------------------------------|
//! | `ext:pdf`, `ext:jpg,png` | names with one of these extensions           |
//! | `type:image`          | a category of extensions (image, video, audio, document, code, archive) |
//! | `type:file`, `type:folder` | only files, or only folders               |
//! | `in:projects`, `in:~/Documents` | the containing path includes this (case-insensitive) |
//! | `size:>10mb`, `size:<1k` | file size (`b`, `k`, `m`, `g`; 1024-based)   |
//! | `modified:<7d`, `modified:>1y` | modified within / longer ago than (`min`, `h`, `d`, `w`, `mo`, `y`) |
//!
//! Extension and kind filters run inside the index scan. Path, size and
//! date filters are checked on the scan's top matches, so the scan
//! over-fetches when they are present. Other words, including unknown
//! `word:value` tokens, stay part of the name being searched.

use std::path::Path;
use std::time::Duration;

use sift_search::{KindFilter, Query, SearchResult};

use crate::FileIndex;

/// Matches fetched from the scan when path, size or date filters are
/// checked afterwards.
pub const POST_FILTER_FETCH: usize = 2000;

const CATEGORIES: &[(&str, &[&str])] = &[
    (
        "image",
        &[
            "jpg", "jpeg", "png", "gif", "heic", "heif", "webp", "tif", "tiff", "bmp", "svg",
            "raw", "dng", "cr2", "cr3", "nef", "arw", "psd",
        ],
    ),
    (
        "video",
        &[
            "mp4", "mov", "m4v", "avi", "mkv", "webm", "wmv", "flv", "mpg", "mpeg",
        ],
    ),
    (
        "audio",
        &[
            "mp3", "m4a", "aac", "wav", "aif", "aiff", "flac", "ogg", "opus", "alac",
        ],
    ),
    (
        "document",
        &[
            "pdf", "doc", "docx", "pages", "txt", "rtf", "md", "odt", "xls", "xlsx", "numbers",
            "csv", "ppt", "pptx", "key", "epub",
        ],
    ),
    (
        "code",
        &[
            "rs", "py", "js", "ts", "tsx", "jsx", "go", "c", "h", "cc", "cpp", "hpp", "m", "java",
            "kt", "swift", "rb", "php", "sh", "zsh", "json", "yaml", "yml", "toml", "html", "css",
            "scss", "sql", "lua",
        ],
    ),
    (
        "archive",
        &[
            "zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", "dmg", "iso", "pkg",
        ],
    ),
];

/// A comparison against a threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound<T> {
    Below(T),
    Above(T),
}

impl<T: PartialOrd> Bound<T> {
    fn admits(&self, value: T) -> bool {
        match self {
            Bound::Below(limit) => value < *limit,
            Bound::Above(limit) => value > *limit,
        }
    }
}

/// A query split into the name to search for and its filters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileQuery {
    /// The words that are not filters.
    pub text: String,
    pub kind: Option<KindFilter>,
    pub extensions: Vec<String>,
    /// Folded fragments the containing path must include (all of them).
    pub within: Vec<String>,
    pub size: Option<Bound<u64>>,
    /// Age since the last modification.
    pub age: Option<Bound<Duration>>,
}

impl FileQuery {
    /// Parses `query`. `Ok(None)` means it has no filters (an ordinary
    /// search); `Err` names a filter whose value is not understood.
    pub fn parse(query: &str) -> Result<Option<Self>, String> {
        let mut parsed = Self::default();
        let mut words = Vec::new();
        let mut any = false;
        for token in query.split_whitespace() {
            let Some((key, value)) = token.split_once(':') else {
                words.push(token);
                continue;
            };
            match key.to_ascii_lowercase().as_str() {
                "ext" => parsed.extensions.extend(parse_extensions(value)?),
                "type" | "kind" => parsed.add_type(value)?,
                "in" => parsed.within.push(parse_within(value)?),
                "size" => parsed.size = Some(parse_size(value)?),
                "modified" | "mod" | "date" => parsed.age = Some(parse_age(value)?),
                _ => {
                    words.push(token);
                    continue;
                }
            }
            any = true;
        }
        if !any {
            return Ok(None);
        }
        parsed.text = words.join(" ");
        Ok(Some(parsed))
    }

    fn add_type(&mut self, value: &str) -> Result<(), String> {
        let value = value.to_ascii_lowercase();
        match value.as_str() {
            "file" | "files" => self.kind = Some(KindFilter::Files),
            "folder" | "folders" | "dir" | "dirs" | "directory" => {
                self.kind = Some(KindFilter::Dirs);
            }
            _ => {
                let name = value.strip_suffix('s').unwrap_or(&value);
                let name = if name == "doc" { "document" } else { name };
                let (_, extensions) = CATEGORIES
                    .iter()
                    .find(|(category, _)| *category == name)
                    .ok_or_else(|| {
                        let known: Vec<&str> = CATEGORIES.iter().map(|(c, _)| *c).collect();
                        format!(
                            "type:{value} is not a known type (file, folder, {})",
                            known.join(", ")
                        )
                    })?;
                self.extensions
                    .extend(extensions.iter().map(|e| (*e).to_owned()));
                self.kind = Some(KindFilter::Files);
            }
        }
        Ok(())
    }

    /// Why `query` cannot run as a filtered search, in words for the
    /// user: a filter value that is not understood, or filters with
    /// nothing to search for. `None` for runnable or unfiltered queries.
    pub fn problem(query: &str) -> Option<String> {
        match Self::parse(query) {
            Err(message) => Some(message),
            Ok(Some(f)) if f.text.trim().is_empty() && f.extensions.is_empty() => {
                Some("Add part of a name to search for, or an ext: or type: filter".into())
            }
            Ok(_) => None,
        }
    }

    /// The index query for these filters, or `None` if there is nothing
    /// to search for. With only an extension, the name searched for is
    /// "." (every name with an extension), narrowed inside the scan.
    pub fn engine_query(&self, limit: usize) -> Option<Query> {
        let text = self.text.trim();
        let (text, fuzzy) = if !text.is_empty() {
            (text, true)
        } else if !self.extensions.is_empty() {
            (".", false)
        } else {
            return None;
        };
        Some(Query {
            text: text.to_owned(),
            limit: if self.needs_post_filter() {
                POST_FILTER_FETCH.max(limit)
            } else {
                limit
            },
            kind: self.kind,
            extensions: self.extensions.clone(),
            fuzzy,
        })
    }

    /// Searches `index` with these filters; up to `limit` results.
    pub fn run(&self, index: &FileIndex, limit: usize) -> Vec<SearchResult> {
        let Some(query) = self.engine_query(limit) else {
            return Vec::new();
        };
        let post = self.needs_post_filter();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        index.search_query(&query, limit, |r| {
            !post || self.admits(&r.path, r.size, r.mtime, now)
        })
    }

    /// Whether path, size or date filters must be checked after the scan.
    pub fn needs_post_filter(&self) -> bool {
        !self.within.is_empty() || self.size.is_some() || self.age.is_some()
    }

    /// Whether a result at `path` with this size and modification time
    /// (Unix seconds) passes the path, size and date filters.
    pub fn admits(&self, path: &Path, size: Option<u64>, mtime: Option<i64>, now: i64) -> bool {
        if !self.within.is_empty() {
            let parent = path
                .parent()
                .map(|p| p.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !self.within.iter().all(|w| parent.contains(w.as_str())) {
                return false;
            }
        }
        if let Some(bound) = self.size {
            // Folders have no meaningful size here.
            if !size.is_some_and(|s| bound.admits(s)) {
                return false;
            }
        }
        if let Some(bound) = self.age {
            let Some(mtime) = mtime else {
                return false;
            };
            let age = Duration::from_secs(now.saturating_sub(mtime).max(0) as u64);
            if !bound.admits(age) {
                return false;
            }
        }
        true
    }
}

fn parse_extensions(value: &str) -> Result<Vec<String>, String> {
    let extensions: Vec<String> = value
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    if extensions.is_empty() {
        return Err("ext: needs an extension, as in ext:pdf".into());
    }
    Ok(extensions)
}

fn parse_within(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("in: needs part of a folder path, as in in:projects".into());
    }
    let expanded = match (value.strip_prefix('~'), std::env::home_dir()) {
        (Some(rest), Some(home)) => format!("{}{rest}", home.display()),
        _ => value.to_owned(),
    };
    Ok(expanded.to_lowercase())
}

/// `>10mb`, `<1k`, `>=2g`, `5m` (same as `>5m`).
fn split_comparison(value: &str) -> (bool, &str) {
    if let Some(rest) = value.strip_prefix(">=").or_else(|| value.strip_prefix('>')) {
        (false, rest)
    } else if let Some(rest) = value.strip_prefix("<=").or_else(|| value.strip_prefix('<')) {
        (true, rest)
    } else {
        (false, value)
    }
}

fn split_number(text: &str) -> Option<(f64, String)> {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let number: f64 = text[..end].parse().ok()?;
    (number.is_finite() && number >= 0.0).then(|| (number, text[end..].to_ascii_lowercase()))
}

fn parse_size(value: &str) -> Result<Bound<u64>, String> {
    let invalid = || format!("size:{value} is not a size; try size:>10mb or size:<500k");
    let (below, rest) = split_comparison(value);
    let (number, unit) = split_number(rest).ok_or_else(invalid)?;
    let scale: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        _ => return Err(invalid()),
    };
    let bytes = (number * scale) as u64;
    Ok(if below {
        Bound::Below(bytes)
    } else {
        Bound::Above(bytes)
    })
}

fn parse_age(value: &str) -> Result<Bound<Duration>, String> {
    let invalid = || format!("modified:{value} is not an age; try modified:<7d or modified:>1y");
    // `<7d`: modified less than 7 days ago. A bare `7d` means the same.
    let (older, rest) = match value.strip_prefix('>') {
        Some(rest) => (true, rest.trim_start_matches('=')),
        None => (
            false,
            value
                .strip_prefix('<')
                .map_or(value, |r| r.trim_start_matches('=')),
        ),
    };
    let (number, unit) = split_number(rest).ok_or_else(invalid)?;
    let seconds: f64 = match unit.as_str() {
        "min" | "mins" => 60.0,
        "h" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86_400.0,
        "w" | "week" | "weeks" => 7.0 * 86_400.0,
        "mo" | "month" | "months" => 30.0 * 86_400.0,
        "y" | "year" | "years" => 365.0 * 86_400.0,
        _ => return Err(invalid()),
    };
    let age = Duration::from_secs_f64(number * seconds);
    Ok(if older {
        Bound::Above(age)
    } else {
        Bound::Below(age)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(query: &str) -> FileQuery {
        FileQuery::parse(query)
            .expect("valid")
            .expect("has filters")
    }

    #[test]
    fn plain_queries_have_no_filters() {
        assert_eq!(FileQuery::parse("quarterly report"), Ok(None));
        // Unknown prefixes are part of the name.
        assert_eq!(FileQuery::parse("re:invent notes"), Ok(None));
    }

    #[test]
    fn splits_text_from_filters() {
        let q = parse("report ext:PDF,.docx in:~/Documents size:>1.5mb modified:<7d");
        assert_eq!(q.text, "report");
        assert_eq!(q.extensions, ["pdf", "docx"]);
        let home = std::env::home_dir().expect("home");
        assert_eq!(
            q.within,
            [format!("{}/documents", home.display()).to_lowercase()]
        );
        assert_eq!(q.size, Some(Bound::Above(1_572_864)));
        assert_eq!(q.age, Some(Bound::Below(Duration::from_secs(7 * 86_400))));
        assert!(q.needs_post_filter());
    }

    #[test]
    fn types_set_kind_and_extensions() {
        assert_eq!(parse("type:folder src").kind, Some(KindFilter::Dirs));
        let images = parse("type:images");
        assert_eq!(images.kind, Some(KindFilter::Files));
        assert!(images.extensions.iter().any(|e| e == "heic"));
        assert_eq!(images.text, "");
        assert!(parse("type:doc").extensions.iter().any(|e| e == "pdf"));
        assert!(!images.needs_post_filter());
    }

    #[test]
    fn problems_are_explained() {
        assert_eq!(FileQuery::problem("report"), None);
        assert_eq!(FileQuery::problem("ext:pdf"), None);
        assert!(FileQuery::problem("in:projects").is_some());
        assert!(FileQuery::problem("size:huge report").is_some());
    }

    #[test]
    fn bad_values_are_reported() {
        for query in [
            "ext:",
            "type:spaceship",
            "size:big",
            "size:>10qb",
            "modified:<soon",
            "in:",
        ] {
            assert!(FileQuery::parse(query).is_err(), "{query}");
        }
    }

    #[test]
    fn post_filters_check_path_size_and_age() {
        let now = 1_000_000;
        let q = parse("x in:projects size:<1k modified:>1d");
        let path = Path::new("/Users/me/Projects/sift/a.txt");
        assert!(q.admits(path, Some(10), Some(now - 2 * 86_400), now));
        assert!(!q.admits(path, Some(4096), Some(now - 2 * 86_400), now));
        assert!(!q.admits(path, Some(10), Some(now - 60), now));
        assert!(!q.admits(Path::new("/tmp/a.txt"), Some(10), Some(0), now));
        assert!(!q.admits(path, None, Some(0), now));
        // The file's own name does not count as its folder.
        let named = Path::new("/tmp/projects.txt");
        assert!(!parse("in:projects").admits(named, Some(1), Some(0), now));
    }
}
