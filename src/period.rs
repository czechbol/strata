use anyhow::Result;
use chrono::{Datelike, TimeZone, Utc};
use git2::{Oid, Repository};
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;
use tracing::debug;

use crate::types::Tag;

// Returns a compact integer key for a Unix timestamp:
//   quarter mode → year * 4 + quarter_index (0-based)
//   year mode    → year * 4 (quarter_index = 0 always, so years sort correctly)
// i32::MIN is the sentinel for unparseable timestamps.
pub fn ts_to_period_int(ts: i64, yearly: bool) -> i32 {
    match Utc.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) => {
            let q = if yearly { 0 } else { (dt.month0() / 3) as i32 };
            dt.year() * 4 + q
        }
        _ => i32::MIN,
    }
}

pub fn period_int_to_string(p: i32, yearly: bool) -> String {
    if p == i32::MIN {
        return "unknown".to_string();
    }
    let year = p / 4;
    if yearly {
        year.to_string()
    } else {
        format!("{}-Q{}", year, (p % 4) + 1)
    }
}

static SEMVER_RE: OnceLock<Regex> = OnceLock::new();

pub fn get_version_tags(repo_path: &Path) -> Result<Vec<Tag>> {
    let repo = Repository::open(repo_path)?;
    let semver_re = SEMVER_RE.get_or_init(|| {
        Regex::new(r"(?i)^refs/tags/v?(\d+)\.(\d+)\.(\d+)").unwrap()
    });

    let mut tag_refs: Vec<(String, Oid)> = Vec::new();
    repo.tag_foreach(|oid, name_bytes| {
        if let Ok(full) = std::str::from_utf8(name_bytes) {
            if semver_re.is_match(full) {
                let short = full.trim_start_matches("refs/tags/").to_string();
                tag_refs.push((short, oid));
            }
        }
        true
    })?;

    let mut raw_tags: Vec<(String, i64)> = tag_refs
        .into_iter()
        .filter_map(|(name, oid)| {
            repo.find_object(oid, None)
                .ok()
                .and_then(|o| o.peel_to_commit().ok())
                .map(|c| (name, c.time().seconds()))
        })
        .collect();

    if raw_tags.is_empty() {
        debug!("No semver tags found");
        return Ok(Vec::new());
    }
    raw_tags.sort_by_key(|t| t.1);
    debug!("{} semver tags found", raw_tags.len());

    let n = raw_tags.len();
    let spacings: Vec<i64> = (0..n)
        .map(|i| {
            let left = if i > 0 { raw_tags[i].1 - raw_tags[i - 1].1 } else { i64::MAX };
            let right = if i + 1 < n { raw_tags[i + 1].1 - raw_tags[i].1 } else { i64::MAX };
            left.min(right).max(0)
        })
        .collect();

    let max_spacing = spacings.iter().copied().max().unwrap_or(1).max(1);

    let tags: Vec<Tag> = raw_tags
        .iter()
        .zip(&spacings)
        .map(|((name, ts), spacing)| Tag {
            name: name.clone(),
            ts: *ts,
            importance: *spacing as f64 / max_spacing as f64,
        })
        .collect();

    let mut top: Vec<_> = tags.iter().collect();
    top.sort_by(|a, b| b.importance.partial_cmp(&a.importance).unwrap());
    for t in top.iter().take(5) {
        debug!("tag {} (importance {:.2})", t.name, t.importance);
    }

    Ok(tags)
}
