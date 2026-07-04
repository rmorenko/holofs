//! server-side catalog filter shared by the
//! [`crate::get_catalog`] and [`crate::list_dir`] server functions.
//!
//! Three URL params drive it:
//! - `q` — name glob with `*` wildcard (regex metacharacters are
//!   escaped so `.` is literal).
//! - `from` / `to` — `YYYY-MM-DD` bounds on `created_at_unix`. Empty
//!   string = no bound.
//!
//! [`CatalogFilter::matches`] runs the filter against one entry; the
//! tree-aware wrapper [`apply_filter`] keeps ancestor directories of
//! any retained leaf so surviving tree paths stay navigable.
//!
//! The date parser is a hand-rolled Howard Hinnant `days_from_civil`
//! adaptation — no `chrono` dependency in the WASM build (this file
//! is SSR-only, but the crate's tests build both targets).
//!

#![cfg(feature = "ssr")]

use crate::catalog_types::CatalogEntry;

/// Server-side filter shared by `get_catalog` + `list_dir`. Built from
/// three URL params: `q` (name glob with `*`), `from` and `to` (date
/// range, `YYYY-MM-DD`).
#[derive(Debug, Clone, Default)]
pub(crate) struct CatalogFilter {
    /// Compiled regex from the glob pattern. `None` means "no name
    /// filter active".
    name_re: Option<regex::Regex>,
    /// Lower bound on `created_at_unix`. `0` = no lower bound.
    from_unix: u64,
    /// Upper bound (exclusive). `0` = no upper bound.
    to_unix_exclusive: u64,
}

impl CatalogFilter {
    /// Parse raw query params; reject malformed dates / regexes with a
    /// user-readable message.
    pub(crate) fn parse(name_glob: &str, from: &str, to: &str) -> Result<Self, String> {
        let name_re = if name_glob.trim().is_empty() {
            None
        } else {
            Some(compile_glob(name_glob.trim()).map_err(|e| format!("bad name filter: {e}"))?)
        };
        let from_unix = parse_date_to_unix(from, false)
            .map_err(|e| format!("bad `from` date: {e}"))?;
        let to_unix_exclusive = parse_date_to_unix(to, true)
            .map_err(|e| format!("bad `to` date: {e}"))?;
        Ok(Self {
            name_re,
            from_unix,
            to_unix_exclusive,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.name_re.is_none() && self.from_unix == 0 && self.to_unix_exclusive == 0
    }

    /// True when `entry` clears every active sub-filter. Legacy
    /// entries with `created_at_unix == 0` (HOLOFSM6/7) are kept
    /// whenever a date filter is set — without a timestamp we'd have
    /// to drop them, which is more confusing than including them.
    pub(crate) fn matches(&self, entry: &CatalogEntry) -> bool {
        if let Some(re) = &self.name_re {
            // Glob runs against the **basename** so `q=*.png` doesn't
            // need to know what folder the file is in. Users who want
            // path-aware matches can prefix `*/`: `*/2026/*` for an
            // explicit directory segment.
            let leaf = entry.name.rsplit('/').next().unwrap_or(&entry.name);
            if !re.is_match(leaf) {
                return false;
            }
        }
        let has_ts = entry.created_at_unix != 0;
        if self.from_unix != 0 && has_ts && entry.created_at_unix < self.from_unix {
            return false;
        }
        if self.to_unix_exclusive != 0 && has_ts && entry.created_at_unix >= self.to_unix_exclusive
        {
            return false;
        }
        true
    }
}

/// Compile a simple glob (`*` = any sequence) into a full-string regex.
/// Every other char is regex-escaped. Patterns are anchored on both
/// sides (`^…$`) so `photo` matches exactly `photo`, not `photograph`.
fn compile_glob(pat: &str) -> Result<regex::Regex, regex::Error> {
    let mut out = String::with_capacity(pat.len() * 2 + 4);
    out.push('^');
    for c in pat.chars() {
        match c {
            '*' => out.push_str(".*"),
            // Regex metacharacters that must be escaped to keep their
            // literal meaning inside the user's filter pattern.
            '.' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' | '^' | '$' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push('$');
    regex::RegexBuilder::new(&out).case_insensitive(true).build()
}

/// Parse `YYYY-MM-DD` to a Unix epoch second. Empty string is a
/// no-op (`0`). `inclusive_end=true` flips the meaning to "end of day"
/// so the upper bound covers the entire `to` day — the filter then
/// stores the value as **exclusive** (start of next day).
fn parse_date_to_unix(s: &str, inclusive_end: bool) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    // Expect strict YYYY-MM-DD; the HTML5 `<input type="date">` always
    // emits this format.
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(format!("expected YYYY-MM-DD, got {s:?}"));
    }
    let y: i64 = parts[0].parse().map_err(|_| format!("bad year in {s:?}"))?;
    let m: u32 = parts[1].parse().map_err(|_| format!("bad month in {s:?}"))?;
    let d: u32 = parts[2].parse().map_err(|_| format!("bad day in {s:?}"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=2999).contains(&y) {
        return Err(format!("out-of-range date {s:?}"));
    }
    let mut secs = ymd_to_unix(y, m, d);
    if inclusive_end {
        // Advance by 86 400s so a `to=` filter covers
        // anything created up to23:59:59 UTC.
        secs = secs.saturating_add(86_400);
    }
    Ok(secs)
}

/// Civil-date → Unix-epoch seconds (UTC). Pure arithmetic, no chrono
/// dependency — Howard Hinnant's `days_from_civil`. All intermediates
/// stay signed so the `m_adj = m + 9` / `m - 3` flip can't wrap when
/// `m <= 2`.
fn ymd_to_unix(y: i64, m: u32, d: u32) -> u64 {
    let m = m as i64;
    let d = d as i64;
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // 0..=399
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_epoch = era * 146_097 + doe - 719_468;
    days_since_epoch.max(0) as u64 * 86_400
}

/// Tree-aware filter. Returns the input minus entries that don't match
/// the filter, **plus** every ancestor-directory entry of any retained
/// leaf so the path is still navigable. Folders that match the filter
/// directly are included regardless of whether they have surviving
/// children.
pub(crate) fn apply_filter(entries: Vec<CatalogEntry>, filter: &CatalogFilter) -> Vec<CatalogEntry> {
    use std::collections::{HashMap, HashSet};

    let mut by_name: HashMap<String, CatalogEntry> = HashMap::with_capacity(entries.len());
    for e in entries.into_iter() {
        by_name.insert(e.name.clone(), e);
    }

    let mut keep: HashSet<String> = HashSet::new();
    for (name, entry) in by_name.iter() {
        if filter.matches(entry) {
            keep.insert(name.clone());
            // Pull every ancestor segment so the tree path survives.
            let mut cur = name.as_str();
            while let Some((parent, _)) = cur.rsplit_once('/') {
                if parent.is_empty() {
                    break;
                }
                if !keep.insert(parent.to_string()) {
                    // Parent already retained — its ancestors are too.
                    break;
                }
                cur = parent;
            }
        }
    }

    let mut out: Vec<CatalogEntry> =
        keep.into_iter().filter_map(|k| by_name.remove(&k)).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: &str, created: u64) -> CatalogEntry {
        CatalogEntry {
            name: name.into(),
            kind: kind.into(),
            content_type: String::new(),
            width: 0,
            height: 0,
            n_shards: 0,
            cid_short: String::new(),
            audio_sample_rate: 0,
            channels: 0,
            created_at_unix: created,
        }
    }

    #[test]
    fn glob_compiles_to_anchored_regex() {
        let re = compile_glob("*.png").unwrap();
        assert!(re.is_match("photo.png"));
        assert!(re.is_match("a.b.png"));
        assert!(!re.is_match("photo.png.bak"));
    }

    #[test]
    fn glob_is_case_insensitive() {
        let re = compile_glob("*.PNG").unwrap();
        assert!(re.is_match("photo.png"));
        assert!(re.is_match("PHOTO.PNG"));
    }

    #[test]
    fn glob_escapes_regex_metachars() {
        let re = compile_glob("a.b").unwrap();
        assert!(re.is_match("a.b"));
        // The `.` is escaped, so it does NOT match any single char.
        assert!(!re.is_match("axb"));
    }

    #[test]
    fn date_parse_ymd_to_unix_round_trips() {
        //= epoch 0.
        assert_eq!(parse_date_to_unix("1970-01-01", false).unwrap(), 0);
        // Inclusive-end flag adds one day (86 400 s).
        assert_eq!(parse_date_to_unix("1970-01-01", true).unwrap(), 86_400);
        // sanity check against the JS Date equivalent
        // (Date.UTC(2026,0,1)/1000 = 1767225600).
        assert_eq!(parse_date_to_unix("2026-01-01", false).unwrap(), 1_767_225_600);
    }

    #[test]
    fn date_parse_rejects_malformed() {
        assert!(parse_date_to_unix("not-a-date", false).is_err());
        assert!(parse_date_to_unix("2026-13-01", false).is_err());
        assert!(parse_date_to_unix("2026-01-32", false).is_err());
        // Empty string is the "no bound" sentinel, not an error.
        assert_eq!(parse_date_to_unix("", false).unwrap(), 0);
    }

    #[test]
    fn filter_matches_combines_name_and_date() {
        let f = CatalogFilter::parse("*.png", "2026-01-01", "2026-12-31").unwrap();
        // Inside both bounds.
        assert!(f.matches(&entry("photo.png", "image", 1_770_000_000)));
        // Wrong extension.
        assert!(!f.matches(&entry("notes.txt", "text", 1_770_000_000)));
        // Before from.
        assert!(!f.matches(&entry("photo.png", "image", 1_000_000_000)));
    }

    #[test]
    fn filter_keeps_legacy_zero_timestamps() {
        // Legacy entries (HOLOFSM6/7) have created_at_unix = 0. They
        // pass any date filter — better to show them than hide them
        // silently.
        let f = CatalogFilter::parse("", "2026-01-01", "2026-12-31").unwrap();
        assert!(f.matches(&entry("anything.txt", "text", 0)));
    }

    #[test]
    fn glob_matches_basename_not_full_path() {
        let f = CatalogFilter::parse("photo*", "", "").unwrap();
        // Glob looks at the leaf, so `Roman/photo.png` should match.
        assert!(f.matches(&entry("Roman/photo.png", "image", 0)));
        assert!(f.matches(&entry("photo_gray.png", "image", 0)));
        assert!(!f.matches(&entry("Roman/notes.txt", "text", 0)));
    }

    #[test]
    fn apply_filter_keeps_ancestor_directories() {
        let mut entries = vec![
            entry("Roman", "directory", 0),
            entry("Roman/sub", "directory", 0),
            entry("Roman/sub/photo.png", "image", 0),
            entry("Roman/sub/notes.txt", "text", 0),
            entry("other.txt", "text", 0),
        ];
        // Glob keeps `*.png` only; ancestor dirs of the surviving leaf
        // ride along so the tree path is still navigable.
        let f = CatalogFilter::parse("*.png", "", "").unwrap();
        let mut kept = apply_filter(std::mem::take(&mut entries), &f);
        kept.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = kept.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Roman", "Roman/sub", "Roman/sub/photo.png"]);
    }
}
