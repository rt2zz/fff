use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::index::constraints::{GlobPattern, compile_one, compiled_matches};
use crate::types::FileItem;

const GLOB_META_CHARS: &[char] = &['*', '?', '[', ']', '{', '}'];

/// Compiled `scan_inclusions` patterns: paths matching any pattern are indexed
/// and watched even when gitignored (like Zed's `file_scan_inclusions`).
#[derive(Default)]
pub(crate) struct ScanInclusions {
    patterns: Vec<String>,
    globs: Vec<GlobPattern>,
    /// Literal prefix of each pattern up to the first glob metachar —
    /// base-relative, '/'-separated walk roots for the supplemental scan.
    roots: Vec<String>,
}

impl ScanInclusions {
    pub(crate) fn new(patterns: &[String]) -> Self {
        let mut compiled = Self::default();
        for raw in patterns {
            let pattern = raw.trim().trim_end_matches('/');
            if pattern.is_empty() {
                continue;
            }
            // `p` matches the path itself, `p/**` everything beneath it, so a
            // plain directory pattern ("docs") includes the whole subtree.
            let globs = compile_one(pattern).zip(compile_one(&format!("{pattern}/**")));
            let Some((exact, subtree)) = globs else {
                tracing::warn!(pattern, "invalid scan_inclusions pattern, skipping");
                continue;
            };
            compiled.globs.extend([exact, subtree]);
            compiled.roots.push(literal_root(pattern));
            compiled.patterns.push(pattern.to_string());
        }
        compiled
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.globs.is_empty()
    }

    pub(crate) fn patterns(&self) -> &[String] {
        &self.patterns
    }

    fn matches_rel(&self, rel: &str) -> bool {
        self.globs.iter().any(|g| compiled_matches(g, rel))
    }

    /// Whether the base-relative `rel` path must not be filtered out: it either
    /// matches a pattern or is an ancestor of a literal pattern root.
    pub(crate) fn reincludes(&self, rel: &Path) -> bool {
        if self.is_empty() {
            return false;
        }
        let rel = crate::path_utils::to_canonical_slashes(&rel.to_string_lossy()).into_owned();
        self.matches_rel(&rel)
            || self.roots.iter().any(|root| {
                root.strip_prefix(rel.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            })
    }

    /// Walk the pattern roots and collect files that match the inclusions but
    /// were skipped by the main (ignore-respecting) walk.
    pub(crate) fn collect_extra_files(
        &self,
        base_path: &Path,
        follow_symlinks: bool,
        existing: &HashSet<&str>,
    ) -> Vec<(FileItem, String)> {
        let mut out = Vec::new();
        for root in walk_roots(&self.roots) {
            self.walk_root(
                &base_path.join(root),
                base_path,
                follow_symlinks,
                existing,
                &mut out,
            );
        }
        // Overlapping roots (e.g. "docs" and "docs/gen/**") can visit the same
        // file twice; the caller re-sorts pairs anyway, so order is free.
        out.sort_unstable_by(|a, b| a.1.cmp(&b.1));
        out.dedup_by(|a, b| a.1 == b.1);
        out
    }

    fn walk_root(
        &self,
        start: &Path,
        base_path: &Path,
        follow_symlinks: bool,
        existing: &HashSet<&str>,
        out: &mut Vec<(FileItem, String)>,
    ) {
        let Ok(start_meta) = std::fs::symlink_metadata(start) else {
            return;
        };
        if start_meta.is_file() {
            return self.push_if_included(start, base_path, Some(&start_meta), existing, out);
        }
        if !start_meta.is_dir() && !(follow_symlinks && start.is_dir()) {
            return;
        }

        // Cycle guard, only reachable when following symlinks.
        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut stack = vec![start.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if follow_symlinks {
                let canonical = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
                if !visited.insert(canonical) {
                    continue;
                }
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_symlink() && !follow_symlinks {
                    continue;
                }
                let is_dir = if file_type.is_symlink() {
                    path.is_dir()
                } else {
                    file_type.is_dir()
                };
                if is_dir {
                    if path.file_name().is_some_and(|n| n == ".git") {
                        continue;
                    }
                    stack.push(path);
                } else {
                    let metadata = entry.metadata().ok();
                    self.push_if_included(&path, base_path, metadata.as_ref(), existing, out);
                }
            }
        }
    }

    fn push_if_included(
        &self,
        path: &Path,
        base_path: &Path,
        metadata: Option<&std::fs::Metadata>,
        existing: &HashSet<&str>,
        out: &mut Vec<(FileItem, String)>,
    ) {
        let (item, rel) = FileItem::new_from_walk(path, base_path, None, metadata);
        if !existing.contains(rel.as_str())
            && self.matches_rel(&rel)
            && !crate::watch::is_git_file(path)
        {
            out.push((item, rel));
        }
    }
}

/// Longest glob-free '/'-prefix of `pattern` ("" when it starts with a glob).
fn literal_root(pattern: &str) -> String {
    pattern
        .split('/')
        .take_while(|c| !c.contains(GLOB_META_CHARS))
        .collect::<Vec<_>>()
        .join("/")
}

/// Unique walk roots, collapsed to the base when any pattern has no literal prefix.
fn walk_roots(roots: &[String]) -> Vec<&str> {
    let mut roots: Vec<&str> = roots.iter().map(String::as_str).collect();
    if roots.iter().any(|r| r.is_empty()) {
        return vec![""];
    }
    roots.sort_unstable();
    roots.dedup();
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn inclusions(patterns: &[&str]) -> ScanInclusions {
        let owned: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        ScanInclusions::new(&owned)
    }

    #[test]
    fn literal_roots_stop_at_glob_metachars() {
        assert_eq!(literal_root("docs/generated/**"), "docs/generated");
        assert_eq!(literal_root(".env"), ".env");
        assert_eq!(literal_root("**/*.secret"), "");
        assert_eq!(literal_root("a/b*.rs"), "a");
    }

    #[test]
    fn walk_roots_dedupe_and_collapse() {
        let roots = vec!["docs".to_string(), "assets".to_string(), "docs".to_string()];
        assert_eq!(walk_roots(&roots), vec!["assets", "docs"]);
        let with_empty = vec!["docs".to_string(), String::new()];
        assert_eq!(walk_roots(&with_empty), vec![""]);
    }

    #[test]
    fn directory_pattern_includes_subtree() {
        let inc = inclusions(&["docs"]);
        assert!(inc.reincludes(Path::new("docs")));
        assert!(inc.reincludes(Path::new("docs/deep/file.md")));
        assert!(!inc.reincludes(Path::new("src/main.rs")));
    }

    #[test]
    fn ancestor_dirs_of_roots_are_reincluded() {
        let inc = inclusions(&["docs/generated/**"]);
        assert!(inc.reincludes(Path::new("docs")));
        assert!(inc.reincludes(Path::new("docs/generated")));
        assert!(inc.reincludes(Path::new("docs/generated/api.md")));
        assert!(!inc.reincludes(Path::new("docs2")));
        assert!(!inc.reincludes(Path::new("src")));
    }

    #[test]
    fn invalid_and_empty_patterns_are_skipped() {
        let inc = inclusions(&["", "  ", "docs/"]);
        assert_eq!(inc.patterns(), &["docs".to_string()]);
    }

    #[test]
    fn collects_only_matching_missing_files_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("secrets/deep")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("secrets/key.env"), "k").unwrap();
        fs::write(root.join("secrets/deep/token.env"), "t").unwrap();
        fs::write(root.join("secrets/readme.md"), "m").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();

        // Overlapping patterns cover the cross-root dedup path.
        let inc = inclusions(&["secrets/**/*.env", "secrets/deep/**"]);
        let mut existing = HashSet::new();
        existing.insert("secrets/key.env");

        let rels: Vec<String> = inc
            .collect_extra_files(root, false, &existing)
            .into_iter()
            .map(|(_, rel)| rel)
            .collect();

        // key.env is already indexed, readme.md and main.rs don't match, and
        // deep/token.env matches both patterns but appears once.
        assert_eq!(rels, vec!["secrets/deep/token.env"]);
    }
}
