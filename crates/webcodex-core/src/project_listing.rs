//! Pure listing logic for `list_project_tracked_files`.
//!
//! The agent runs one deterministic `git ls-files -z` and returns raw bytes.
//! Everything that decides what the model sees — scope, glob filter, directory
//! rollup, pagination — happens here, so it is unit-testable without an agent.
//!
//! Rollup is what keeps this usable on a large repository. A flat list of
//! 50,000 tracked paths is not a project structure, it is a wall. When the
//! flat list does not fit the caller's limit, the deepest directory depth that
//! *does* fit is chosen automatically and reported back, so one call always
//! returns a complete picture at some resolution instead of an arbitrary
//! prefix of an incomplete one.
//!
//! That guarantee depends on complete source acquisition. Callers that hit a
//! producer/source bound must expose `list_truncated=true` and must not present
//! this ordinary offset pagination as a way to traverse the full repository.

use serde_json::{json, Value};

/// Deepest rollup depth the automatic search will consider. Beyond this, extra
/// depth stops buying legibility and only costs entries.
const MAX_AUTO_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "dir",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingEntry {
    pub path: String,
    pub kind: EntryKind,
    /// Number of tracked files beneath a rolled-up directory. `None` for files.
    pub file_count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<ListingEntry>,
    /// Files matching scope + globs, before rollup and pagination.
    pub total_files: usize,
    /// Entries after rollup, before pagination.
    pub total_entries: usize,
    /// Effective rollup depth; `None` means every file is listed individually.
    pub depth: Option<usize>,
    /// True when `depth` was chosen automatically rather than requested.
    pub depth_auto: bool,
    pub truncated: bool,
    pub next_offset: Option<usize>,
}

/// Split a `git ls-files -z` byte stream into paths.
///
/// Returns `(paths, truncated)`. Every complete record ends with NUL, so a
/// non-empty tail without one is an incomplete source-acquisition final path
/// and is dropped rather than reported as a real file — a half path would send the
/// model to read something that does not exist.
pub fn parse_nul_separated(raw: &str) -> (Vec<String>, bool) {
    if raw.is_empty() {
        return (Vec::new(), false);
    }
    let truncated = !raw.ends_with('\0');
    let mut paths: Vec<String> = raw
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect();
    if truncated {
        paths.pop();
    }
    (paths, truncated)
}

/// Normalize a caller-supplied scope into a `dir/`-shaped prefix.
/// `""`, `"."`, and `"./"` all mean the project root.
pub fn normalize_scope(path: Option<&str>) -> String {
    let raw = path.unwrap_or("").trim();
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return String::new();
    }
    format!("{}/", trimmed.trim_start_matches("./"))
}

/// Match one project-relative path against a glob.
///
/// Supported: `*` (any run except `/`), `?` (one character except `/`), `**`
/// (any run including `/`). Deliberately no character classes or braces —
/// a pattern language the model can predict beats an expressive one it cannot.
///
/// A pattern containing no `/` also matches the basename, so `*.py` works
/// without writing `**/*.py`.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    if !pattern.contains('/') {
        let basename = path.rsplit('/').next().unwrap_or(path);
        if glob_match_segment(pattern.as_bytes(), basename.as_bytes()) {
            return true;
        }
    }
    glob_match_segment(pattern.as_bytes(), path.as_bytes())
}

/// Backtracking glob matcher over bytes.
///
/// Iterative rather than recursive so a hostile pattern like `**a**a**a**`
/// cannot blow the call stack. Every encountered star keeps one bounded
/// backtrack frame: a later single `*` may get stuck at `/`, in which case the
/// matcher must still be able to widen an earlier `**` (for example `**/*`
/// against `a/b/c`).
fn glob_match_segment(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    // (pattern index just after the star, next text index consumed by that
    // star, whether the star may cross `/`). The number of frames is bounded
    // by the pattern length (itself capped by the public input contract).
    let mut stars: Vec<(usize, usize, bool)> = Vec::new();
    while t < text.len() {
        if p < pattern.len() {
            match pattern[p] {
                b'*' => {
                    let crosses_slash = pattern.get(p + 1) == Some(&b'*');
                    p += if crosses_slash { 2 } else { 1 };
                    stars.push((p, t, crosses_slash));
                    continue;
                }
                b'?' if text[t] != b'/' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                byte if byte == text[t] => {
                    p += 1;
                    t += 1;
                    continue;
                }
                _ => {}
            }
        }

        let mut resumed = false;
        while let Some((resume_p, resume_t, crosses_slash)) = stars.last_mut() {
            if *resume_t < text.len() && (*crosses_slash || text[*resume_t] != b'/') {
                *resume_t += 1;
                p = *resume_p;
                t = *resume_t;
                resumed = true;
                break;
            }
            stars.pop();
        }
        if !resumed {
            return false;
        }
    }
    while pattern.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pattern.len()
}

/// Number of entries a rollup at `depth` would produce for `paths`, where each
/// path is already relative to the requested scope.
fn entry_count_at_depth(relatives: &[&str], depth: usize) -> usize {
    let mut keys: Vec<&str> = relatives.iter().map(|rel| rollup_key(rel, depth)).collect();
    keys.sort_unstable();
    keys.dedup();
    keys.len()
}

/// The entry a scope-relative path collapses into at `depth`.
///
/// A path with more than `depth` segments collapses onto its `depth`-segment
/// ancestor directory (trailing slash kept, so the caller can tell a rolled-up
/// directory from a file). A path already at or above the depth keeps its own
/// identity — `README.md` stays a file even at depth 1.
fn rollup_key(relative: &str, depth: usize) -> &str {
    // Segment count exceeds `depth` exactly when a `depth`-th separator exists.
    match relative
        .match_indices('/')
        .nth(depth.saturating_sub(1))
        .map(|(index, _)| index)
    {
        Some(index) => &relative[..=index],
        None => relative,
    }
}

/// Build the listing the model sees.
///
/// `paths` are project-relative tracked files. `scope` is a `dir/`-shaped
/// prefix (or empty for the root). When `depth` is `None` the deepest depth
/// that fits `limit` is chosen automatically.
pub fn build_listing(
    paths: &[String],
    scope: &str,
    globs: &[String],
    depth: Option<usize>,
    limit: usize,
    offset: usize,
) -> Listing {
    let limit = limit.max(1);
    let mut matched: Vec<&str> = paths
        .iter()
        // Use the shared policy: sensitive paths stay hidden like they do for
        // reads, writes, and edits, while bulk trees stay hidden like they do
        // for `files_search`. A tracked `.env` is a real thing: without this
        // the listing advertises a path that `files_read` then refuses, which
        // costs a round trip and gives the model a contradictory signal.
        .filter(|path| !crate::sensitive_paths::is_bulk_skipped_path(path))
        .filter(|path| scope.is_empty() || path.starts_with(scope))
        .filter(|path| {
            globs.is_empty() || globs.iter().any(|glob| glob_matches(glob, path.as_str()))
        })
        .map(String::as_str)
        .collect();
    matched.sort_unstable();
    matched.dedup();
    let total_files = matched.len();

    // Depth is counted inside the requested scope, so `path=src, depth=1`
    // shows `src/db/` rather than collapsing everything back to `src/`.
    let relatives: Vec<&str> = matched
        .iter()
        .map(|path| &path[scope.len().min(path.len())..])
        .collect();

    let (effective_depth, depth_auto) = match depth {
        Some(requested) => (Some(requested.max(1)), false),
        None if total_files <= limit => (None, false),
        None => (Some(auto_depth(&relatives, limit)), true),
    };

    let mut entries: Vec<ListingEntry> = match effective_depth {
        None => matched
            .iter()
            .map(|path| ListingEntry {
                path: (*path).to_string(),
                kind: EntryKind::File,
                file_count: None,
            })
            .collect(),
        Some(depth) => {
            let mut rolled: Vec<(&str, usize)> = Vec::new();
            for relative in &relatives {
                let key = rollup_key(relative, depth);
                match rolled.last_mut() {
                    Some((last, count)) if *last == key => *count += 1,
                    _ => rolled.push((key, 1)),
                }
            }
            rolled
                .into_iter()
                .map(|(key, count)| {
                    let is_dir = key.ends_with('/');
                    ListingEntry {
                        path: format!("{scope}{key}"),
                        kind: if is_dir {
                            EntryKind::Dir
                        } else {
                            EntryKind::File
                        },
                        file_count: is_dir.then_some(count),
                    }
                })
                .collect()
        }
    };

    let total_entries = entries.len();
    let start = offset.min(total_entries);
    entries.drain(..start);
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    Listing {
        entries,
        total_files,
        total_entries,
        depth: effective_depth,
        depth_auto,
        truncated,
        next_offset: truncated.then_some(start + limit),
    }
}

/// Deepest depth whose entry count still fits `limit`.
///
/// Falls back to depth 1 when even the top level overflows: the caller then
/// pages through top-level entries, which is the honest answer for a
/// repository with more top-level directories than the page size.
fn auto_depth(relatives: &[&str], limit: usize) -> usize {
    let deepest = relatives
        .iter()
        .map(|relative| relative.matches('/').count() + 1)
        .max()
        .unwrap_or(1)
        .min(MAX_AUTO_DEPTH);
    for depth in (1..=deepest).rev() {
        if entry_count_at_depth(relatives, depth) <= limit {
            return depth;
        }
    }
    1
}

impl Listing {
    pub fn to_json(&self, project: &str, scope: &str, list_truncated: bool) -> Value {
        // Offset pagination is authoritative only when the source acquisition
        // was complete. A bounded source prefix may still provide useful
        // structure, but advertising `next_offset` over that prefix would imply
        // the caller can eventually traverse files that were never acquired.
        let page_truncated = self.truncated && !list_truncated;
        let next_offset = if list_truncated {
            None
        } else {
            self.next_offset
        };
        json!({
            "project": project,
            "path": scope.trim_end_matches('/'),
            "entries": self
                .entries
                .iter()
                .map(|entry| {
                    let mut value = json!({ "path": entry.path, "kind": entry.kind.as_str() });
                    if let Some(count) = entry.file_count {
                        value["file_count"] = json!(count);
                    }
                    value
                })
                .collect::<Vec<_>>(),
            "returned": self.entries.len(),
            "total_files": self.total_files,
            "total_entries": self.total_entries,
            "depth": self.depth,
            "depth_auto": self.depth_auto,
            "truncated": page_truncated,
            "next_offset": next_offset,
            // Source acquisition ended before the Git index did, so the totals
            // undercount. Distinct from `truncated`, which is safe paging over a
            // complete source and is therefore suppressed while this is true.
            "list_truncated": list_truncated,
            "source": "git_index",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_string()).collect()
    }

    #[test]
    fn credentials_and_bulk_trees_never_reach_the_model() {
        // Git tracks whatever it was told to track, including secrets. The
        // listing must not be the one file tool that hands them over.
        let tracked = paths(&[
            "src/main.rs",
            ".env",
            ".env.production",
            "deploy/runner.toml",
            "deploy/agent.toml",
            "certs/server.pem",
            "certs/server.key",
            "secrets/db.txt",
            "project-registry/one.toml",
            "projects.d/one.toml",
            "node_modules/left-pad/index.js",
            "target/debug/build.rs",
        ]);
        let listing = build_listing(&tracked, "", &[], None, 100, 0);
        let shown: Vec<&str> = listing
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(shown, vec!["src/main.rs"], "leaked: {shown:?}");
        // The count is what survived the filter, not what git printed —
        // otherwise `total_files` silently advertises hidden paths.
        assert_eq!(listing.total_files, 1);
    }

    #[test]
    fn an_explicit_glob_does_not_reopen_the_filtered_paths() {
        // Asking for the secret by name is still asking for the secret.
        let tracked = paths(&["src/main.rs", ".env", "certs/server.key"]);
        let listing = build_listing(
            &tracked,
            "",
            &[".env".to_string(), "**/*.key".to_string()],
            None,
            100,
            0,
        );
        assert!(listing.entries.is_empty(), "{:?}", listing.entries);
        assert_eq!(listing.total_files, 0);
    }

    #[test]
    fn a_rolled_up_directory_does_not_count_files_it_hides() {
        // file_count is a claim about what a caller would find by descending.
        // Counting filtered paths would make that claim false.
        let tracked = paths(&["app/main.rs", "app/util.rs", "app/.env", "app/id.pem"]);
        let listing = build_listing(&tracked, "", &[], Some(1), 100, 0);
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].path, "app/");
        assert_eq!(listing.entries[0].file_count, Some(2));
    }

    #[test]
    fn a_truncated_final_record_is_dropped_not_reported_as_a_file() {
        let (complete, truncated) = parse_nul_separated("a.rs\0b.rs\0");
        assert_eq!(complete, vec!["a.rs", "b.rs"]);
        assert!(!truncated);

        // No trailing NUL: the last path is a fragment of a real one.
        let (partial, truncated) = parse_nul_separated("a.rs\0src/very-long-na");
        assert_eq!(partial, vec!["a.rs"]);
        assert!(truncated);
    }

    #[test]
    fn scope_normalization_treats_root_spellings_alike() {
        for spelling in [None, Some(""), Some("."), Some("/"), Some("./")] {
            assert_eq!(normalize_scope(spelling), "", "{spelling:?}");
        }
        assert_eq!(normalize_scope(Some("src")), "src/");
        assert_eq!(normalize_scope(Some("/src/")), "src/");
        assert_eq!(normalize_scope(Some("src/db")), "src/db/");
    }

    #[test]
    fn a_bare_pattern_matches_the_basename_at_any_depth() {
        assert!(glob_matches("*.py", "train.py"));
        assert!(glob_matches("*.py", "src/models/train.py"));
        assert!(!glob_matches("*.py", "src/train.pyc"));
    }

    #[test]
    fn a_single_star_stops_at_a_slash_and_a_double_star_crosses_it() {
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("src/*.rs", "src/db/main.rs"));
        assert!(glob_matches("src/**/*.rs", "src/db/main.rs"));
        assert!(glob_matches("src/**", "src/db/main.rs"));
        assert!(glob_matches("?.rs", "a.rs"));
        assert!(!glob_matches("?.rs", "ab.rs"));
    }

    #[test]
    fn an_earlier_double_star_can_widen_after_a_later_single_star_hits_a_slash() {
        assert!(glob_matches("**/*", "a/b/c"));
        assert!(glob_matches("src/**/*", "src/a/b/c.rs"));
        assert!(!glob_matches("src/*/*", "src/a/b/c.rs"));
    }

    #[test]
    fn a_pathological_pattern_terminates_without_recursing() {
        // Would be exponential under naive backtracking recursion.
        let pattern = "**a**a**a**a**a**a**b";
        let text = "a".repeat(2048);
        assert!(!glob_matches(pattern, &text));
    }

    #[test]
    fn a_small_project_is_returned_whole_and_flat() {
        let files = paths(&["README.md", "src/main.rs", "src/db/mod.rs"]);
        let listing = build_listing(&files, "", &[], None, 50, 0);

        assert_eq!(listing.depth, None, "a list that fits must not roll up");
        assert!(!listing.depth_auto);
        assert_eq!(listing.entries.len(), 3);
        assert_eq!(listing.total_files, 3);
        assert!(!listing.truncated);
        assert!(listing.entries.iter().all(|e| e.kind == EntryKind::File));
    }

    #[test]
    fn a_project_too_large_to_list_rolls_up_to_the_deepest_depth_that_fits() {
        // 3 top-level dirs; 60 files spread two levels deep.
        let mut files = Vec::new();
        for top in ["alpha", "beta", "gamma"] {
            for mid in 0..4 {
                for leaf in 0..5 {
                    files.push(format!("{top}/m{mid}/f{leaf}.rs"));
                }
            }
        }
        assert_eq!(files.len(), 60);

        // Depth 2 yields 12 dirs, depth 1 yields 3. With limit 12 the deeper
        // one still fits and must win — more resolution for the same budget.
        let listing = build_listing(&files, "", &[], None, 12, 0);
        assert_eq!(listing.depth, Some(2));
        assert!(listing.depth_auto);
        assert_eq!(listing.entries.len(), 12);
        assert!(!listing.truncated);
        assert_eq!(listing.total_files, 60);
        assert_eq!(listing.entries[0].path, "alpha/m0/");
        assert_eq!(listing.entries[0].kind, EntryKind::Dir);
        assert_eq!(listing.entries[0].file_count, Some(5));
        // Every file is accounted for by exactly one rolled-up directory.
        let counted: usize = listing.entries.iter().filter_map(|e| e.file_count).sum();
        assert_eq!(counted, 60);

        // A tighter budget forces the shallower depth rather than a truncated
        // deep list, so the model still sees the whole project.
        let listing = build_listing(&files, "", &[], None, 5, 0);
        assert_eq!(listing.depth, Some(1));
        assert_eq!(listing.entries.len(), 3);
        assert!(!listing.truncated);
    }

    #[test]
    fn shallow_files_keep_their_identity_when_deeper_siblings_roll_up() {
        let files = paths(&["README.md", "src/main.rs", "src/db/mod.rs", "src/db/row.rs"]);
        let listing = build_listing(&files, "", &[], Some(1), 50, 0);

        assert_eq!(listing.entries.len(), 2);
        assert_eq!(listing.entries[0].path, "README.md");
        assert_eq!(listing.entries[0].kind, EntryKind::File);
        assert_eq!(listing.entries[1].path, "src/");
        assert_eq!(listing.entries[1].file_count, Some(3));
    }

    #[test]
    fn depth_is_counted_inside_the_requested_scope() {
        let files = paths(&[
            "src/db/mod.rs",
            "src/db/row.rs",
            "src/http/mod.rs",
            "top.rs",
        ]);
        let listing = build_listing(&files, "src/", &[], Some(1), 50, 0);

        // Scoped to src/, depth 1 shows src's children, not src itself.
        assert_eq!(listing.total_files, 3, "top.rs is outside the scope");
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/db/", "src/http/"]
        );
        assert_eq!(listing.entries[0].file_count, Some(2));
    }

    #[test]
    fn globs_filter_before_rollup_so_counts_describe_what_matched() {
        let files = paths(&[
            "src/a.rs",
            "src/b.py",
            "src/deep/c.rs",
            "src/deep/d.py",
            "docs/e.py",
        ]);
        let listing = build_listing(&files, "", &["*.py".to_string()], Some(1), 50, 0);

        assert_eq!(listing.total_files, 3);
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| (e.path.as_str(), e.file_count))
                .collect::<Vec<_>>(),
            vec![("docs/", Some(1)), ("src/", Some(2))]
        );
    }

    #[test]
    fn pagination_reports_the_offset_that_continues_the_page() {
        let files: Vec<String> = (0..10).map(|index| format!("f{index:02}.rs")).collect();

        let first = build_listing(&files, "", &[], None, 4, 0);
        assert_eq!(first.entries.len(), 4);
        assert!(first.truncated);
        assert_eq!(first.next_offset, Some(4));
        assert_eq!(first.total_entries, 10);

        let second = build_listing(&files, "", &[], None, 4, first.next_offset.unwrap());
        assert_eq!(second.entries[0].path, "f04.rs");
        assert_eq!(second.next_offset, Some(8));

        let last = build_listing(&files, "", &[], None, 4, 8);
        assert_eq!(last.entries.len(), 2);
        assert!(!last.truncated);
        assert_eq!(last.next_offset, None);
    }

    #[test]
    fn an_offset_past_the_end_returns_nothing_rather_than_wrapping() {
        let files = paths(&["a.rs", "b.rs"]);
        let listing = build_listing(&files, "", &[], None, 10, 99);
        assert!(listing.entries.is_empty());
        assert!(!listing.truncated);
        assert_eq!(listing.next_offset, None);
    }

    #[test]
    fn more_top_level_entries_than_the_limit_pages_instead_of_hiding_them() {
        let files: Vec<String> = (0..30).map(|index| format!("d{index:02}/f.rs")).collect();
        let listing = build_listing(&files, "", &[], None, 10, 0);

        assert_eq!(listing.depth, Some(1), "cannot fit even at depth 1");
        assert_eq!(listing.total_entries, 30);
        assert_eq!(listing.entries.len(), 10);
        assert!(listing.truncated);
        assert_eq!(listing.next_offset, Some(10));
    }

    #[test]
    fn source_incomplete_json_never_advertises_offset_continuation() {
        let files: Vec<String> = (0..10).map(|index| format!("f{index:02}.rs")).collect();
        let listing = build_listing(&files, "", &[], None, 4, 0);
        assert!(listing.truncated);
        assert_eq!(listing.next_offset, Some(4));

        let value = listing.to_json("proj", "", true);
        assert_eq!(value["returned"], 4);
        assert_eq!(value["list_truncated"], true);
        assert_eq!(value["truncated"], false);
        assert_eq!(value["next_offset"], Value::Null);
    }

    #[test]
    fn json_reports_the_scope_without_a_trailing_slash_and_flags_list_truncation() {
        let files = paths(&["src/a.rs"]);
        let value = build_listing(&files, "src/", &[], None, 10, 0).to_json("proj", "src/", true);

        assert_eq!(value["path"], "src");
        assert_eq!(value["project"], "proj");
        assert_eq!(value["list_truncated"], true);
        assert_eq!(value["depth"], Value::Null);
        assert_eq!(value["entries"][0]["path"], "src/a.rs");
        assert_eq!(value["entries"][0]["kind"], "file");
        assert!(value["entries"][0].get("file_count").is_none());
        assert_eq!(value["source"], "git_index");
    }
}
