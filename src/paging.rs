//! Walking through photos oldest-first, one page at a time.
//!
//! Pages are found by date ("everything taken after X"), not by page
//! number. Page numbers would shift every time you delete something: delete
//! 5 photos on page 1 and the first 5 photos of page 2 slide back onto page 1,
//! where you'd never see them. A date-based cursor doesn't move.
//!
//! Several photos can share the exact same timestamp (burst shots), so the
//! cursor also remembers which photos at that timestamp were already shown.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use std::collections::HashSet;

use crate::immich::{Asset, PhotoSource, SourceError, MAX_SEARCH_SIZE};

/// Where a page starts. The default cursor means "from the very oldest".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    /// Show photos taken after this moment…
    pub after: Option<DateTime<Utc>>,
    /// …plus photos taken exactly at `after` that are not in this list.
    #[serde(default)]
    pub seen_at_boundary: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct Page {
    pub items: Vec<Asset>,
    /// Where the following page starts, or None at the newest photo.
    pub next: Option<Cursor>,
}

/// Ask Immich for more than one page, so photos filtered out at the
/// boundary still leave a full page.
#[must_use]
pub fn fetch_size(page_size: usize) -> usize {
    (page_size * 2 + 50).min(MAX_SEARCH_SIZE)
}

/// One page of photos starting at `cursor`.
///
/// # Errors
/// If Immich can't be asked, or more photos than one search can return share
/// the cursor's timestamp.
pub async fn load_page(
    source: &dyn PhotoSource,
    cursor: &Cursor,
    page_size: usize,
) -> Result<Page, SourceError> {
    // Ask from just before the cursor, in case Immich treats "after" as
    // strictly-after. `select_page` then filters exactly.
    let query_after = cursor.after.map(|t| t - Duration::milliseconds(1));
    let mut fetch = fetch_size(page_size);
    loop {
        let found = source.assets_taken_after(query_after, fetch).await?;
        let fetched_full = found.returned >= fetch;
        let mut candidates = found.assets;
        candidates.sort_by(|a, b| a.taken_at.cmp(&b.taken_at).then_with(|| a.id.cmp(&b.id)));
        let page = select_page(cursor, candidates, page_size, fetched_full);
        if !page.items.is_empty() || !fetched_full {
            return Ok(page);
        }
        // Everything fetched was already shown, but Immich has more: a burst
        // bigger than the fetch, all at the cursor's timestamp. Ask for as
        // many as Immich allows before giving up.
        if fetch >= MAX_SEARCH_SIZE {
            return Err(SourceError::BadResponse(format!(
                "more than {MAX_SEARCH_SIZE} photos share the timestamp {}, so the tool can't page past them",
                cursor.after.map(|t| t.to_rfc3339()).unwrap_or_default()
            )));
        }
        fetch = MAX_SEARCH_SIZE;
    }
}

/// Pure logic, no I/O: pick the next `page_size` photos after `cursor` from
/// `candidates` (sorted oldest first).
pub fn select_page(
    cursor: &Cursor,
    candidates: Vec<Asset>,
    page_size: usize,
    fetched_full: bool,
) -> Page {
    let seen_before: HashSet<&str> = cursor.seen_at_boundary.iter().map(String::as_str).collect();
    let fresh: Vec<Asset> = candidates
        .into_iter()
        .filter(|a| match cursor.after {
            None => true,
            Some(t) => a.taken_at > t || (a.taken_at == t && !seen_before.contains(a.id.as_str())),
        })
        .collect();

    let more_after_this_page = fresh.len() > page_size || fetched_full;
    let items: Vec<Asset> = fresh.into_iter().take(page_size).collect();

    let next = match items.last() {
        Some(last) if more_after_this_page => {
            let t = last.taken_at;
            let mut seen: Vec<String> = items
                .iter()
                .filter(|a| a.taken_at == t)
                .map(|a| a.id.clone())
                .collect();
            // A burst that spans several pages: keep what earlier pages showed.
            // (This page only shows photos not seen before, so no duplicates.)
            if cursor.after == Some(t) {
                seen.extend(cursor.seen_at_boundary.iter().cloned());
            }
            Some(Cursor {
                after: Some(t),
                seen_at_boundary: seen,
            })
        }
        _ => None,
    };

    Page { items, next }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn asset(id: &str, secs: i64) -> Asset {
        Asset {
            id: id.to_string(),
            taken_at: Utc.timestamp_opt(1_000_000_000 + secs, 0).unwrap(),
            local_date_time: None,
            file_name: format!("{id}.jpg"),
            is_video: false,
            place: None,
        }
    }

    fn ids(page: &Page) -> Vec<&str> {
        page.items.iter().map(|a| a.id.as_str()).collect()
    }

    #[test]
    fn first_page_starts_at_the_oldest() {
        let all = vec![asset("a", 1), asset("b", 2), asset("c", 3)];
        let page = select_page(&Cursor::default(), all, 2, false);
        assert_eq!(ids(&page), ["a", "b"]);
        let next = page.next.unwrap();
        assert_eq!(next.after, Some(asset("b", 2).taken_at));
        assert_eq!(next.seen_at_boundary, ["b"]);
    }

    #[test]
    fn next_page_continues_after_the_cursor() {
        let cursor = Cursor {
            after: Some(asset("b", 2).taken_at),
            seen_at_boundary: vec!["b".into()],
        };
        let candidates = vec![asset("b", 2), asset("c", 3), asset("d", 4)];
        let page = select_page(&cursor, candidates, 10, false);
        assert_eq!(ids(&page), ["c", "d"]);
        assert_eq!(page.next, None, "nothing newer, so this is the last page");
    }

    #[test]
    fn burst_split_across_pages_shows_every_photo_once() {
        // Four photos with the same timestamp, two per page.
        let burst = || vec![asset("p", 5), asset("q", 5), asset("r", 5), asset("s", 5)];

        let p1 = select_page(&Cursor::default(), burst(), 2, false);
        assert_eq!(ids(&p1), ["p", "q"]);

        let p2 = select_page(p1.next.as_ref().unwrap(), burst(), 2, false);
        assert_eq!(ids(&p2), ["r", "s"]);
    }

    #[test]
    fn deleting_photos_does_not_skip_any() {
        // Page 1 showed a, b. The user trashed b. Reloading page 1 with the
        // same cursor must now pull c in, not jump past it.
        let start = Cursor::default();
        let after_delete = vec![asset("a", 1), asset("c", 3), asset("d", 4)];
        let page = select_page(&start, after_delete, 2, false);
        assert_eq!(ids(&page), ["a", "c"]);
    }

    #[test]
    fn full_fetch_means_there_may_be_more() {
        let page = select_page(&Cursor::default(), vec![asset("a", 1)], 5, true);
        assert!(page.next.is_some());
    }

    #[test]
    fn empty_library_gives_empty_page() {
        let page = select_page(&Cursor::default(), vec![], 24, false);
        assert!(page.items.is_empty());
        assert_eq!(page.next, None);
    }

    #[test]
    fn fetch_size_respects_immich_limit() {
        assert_eq!(fetch_size(24), 98);
        assert_eq!(fetch_size(5000), MAX_SEARCH_SIZE);
    }
}
