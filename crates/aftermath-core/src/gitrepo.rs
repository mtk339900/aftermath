//! Repository inspection built on libgit2 (via `git2`).
//!
//! Aftermath never shells out to `git`: this machine has no git binary, and a
//! library keeps analysis offline, injection-free and dependency-free. The
//! module is read-only with respect to the user's repository; the only write
//! helpers are for building throwaway test fixtures (see [`testkit`]).

use std::path::{Path, PathBuf};

use git2::{DiffOptions, Repository, Tree};

use crate::report::ScopeMode;

/// Hunks for one file: `(path, new-side lines, old-side lines)`.
type FileHunks = (String, Vec<u32>, Vec<u32>);

/// Errors surfaced by repository operations.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not a git repository (searched from {0})")]
    NotFound(String),
    #[error("git error: {0}")]
    Git(#[from] git2::Error),
    #[error("revision not found: {0}")]
    RevisionNotFound(String),
    #[error("path is not inside the repository: {0}")]
    PathOutside(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// One file's change within the analyzed scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Repository-relative path (`/`-separated) after the change.
    pub path: String,
    /// Previous path, for renames.
    pub old_path: Option<String>,
    pub category: crate::evidence::ChangeCategory,
    /// Content before the change (`None` for added files).
    pub old_content: Option<String>,
    /// Content after the change (`None` for deleted files).
    pub new_content: Option<String>,
    /// 1-based new-side lines touched (context lines excluded).
    pub new_lines: Vec<u32>,
    /// 1-based old-side lines touched (context lines excluded).
    pub old_lines: Vec<u32>,
    /// Delta was marked binary (content not read).
    pub binary: bool,
}

/// Open repository handle rooted at `start` (or its parents).
pub struct GitRepo {
    repo: Repository,
    root: PathBuf,
}

impl GitRepo {
    pub fn discover(start: &Path) -> Result<Self, GitError> {
        let repo = Repository::discover(start)
            .map_err(|_| GitError::NotFound(start.display().to_string()))?;
        let root = repo
            .workdir()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| GitError::NotFound(start.display().to_string()))?;
        Ok(Self { repo, root })
    }

    /// Initialize a new repository (used by fixtures and `aftermath init`
    /// in a directory that is not yet versioned — never automatic).
    pub fn init(path: &Path) -> Result<Self, GitError> {
        let repo = Repository::init(path).map_err(GitError::Git)?;
        let root = repo
            .workdir()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| GitError::NotFound(path.display().to_string()))?;
        Ok(Self { repo, root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repository(&self) -> &Repository {
        &self.repo
    }

    /// HEAD object id as a hex string, when it resolves.
    pub fn head_oid(&self) -> Option<String> {
        self.repo.head().ok()?.target().map(|o| o.to_string())
    }

    /// Whether the working tree differs from HEAD (untracked files included).
    pub fn is_dirty(&self) -> bool {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false);
        match self.repo.statuses(Some(&mut opts)) {
            Ok(statuses) => !statuses.is_empty(),
            Err(_) => false,
        }
    }

    /// Tree id of HEAD, when it resolves (`None` for an unborn branch).
    fn head_tree_id(&self) -> Result<Option<git2::Oid>, GitError> {
        match self.repo.head() {
            Ok(head) => {
                let commit = head.peel_to_commit()?;
                Ok(Some(commit.tree()?.id()))
            }
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => Ok(None),
            Err(e) => Err(GitError::Git(e)),
        }
    }

    fn tree_of(&self, id: git2::Oid) -> Result<Tree<'_>, GitError> {
        Ok(self.repo.find_tree(id)?)
    }

    fn empty_tree(&self) -> Result<Tree<'_>, GitError> {
        let oid = self.repo.treebuilder(None)?.write()?;
        Ok(self.repo.find_tree(oid)?)
    }

    /// Collect changed files for the requested scope.
    ///
    /// * `Worktree` — net change of the working tree against HEAD
    ///   (staged + unstaged combined).
    /// * `Staged` — index against HEAD.
    /// * `Revision` — the commit against its first parent.
    pub fn changes(
        &self,
        mode: ScopeMode,
        revision: Option<&str>,
    ) -> Result<Vec<Change>, GitError> {
        let mut opts = DiffOptions::new();
        opts.context_lines(0); // exact changed lines, no context padding
        opts.include_unmodified(false);
        opts.include_untracked(true); // new files are changes too

        let mut changes: Vec<Change> = Vec::new();

        match mode {
            ScopeMode::Worktree => {
                let head = self.head_tree_id()?;
                let empty;
                let base: &Tree<'_> = match head {
                    Some(id) => {
                        empty = self.tree_of(id)?;
                        &empty
                    }
                    None => {
                        empty = self.empty_tree()?;
                        &empty
                    }
                };
                let mut diff = self
                    .repo
                    .diff_tree_to_workdir_with_index(Some(base), Some(&mut opts))?;
                let _ = diff.find_similar(None); // rename detection (best effort)
                let mut hunks = Self::gather_hunks(&diff)?;
                for delta in diff.deltas() {
                    if let Some(c) = self.build_change(&delta, &mut hunks, true)? {
                        changes.push(c);
                    }
                }
            }
            ScopeMode::Staged => {
                let head = self.head_tree_id()?;
                let empty;
                let base: &Tree<'_> = match head {
                    Some(id) => {
                        empty = self.tree_of(id)?;
                        &empty
                    }
                    None => {
                        empty = self.empty_tree()?;
                        &empty
                    }
                };
                let mut diff = self
                    .repo
                    .diff_tree_to_index(Some(base), None, Some(&mut opts))?;
                let _ = diff.find_similar(None);
                let mut hunks = Self::gather_hunks(&diff)?;
                for delta in diff.deltas() {
                    if let Some(c) = self.build_change(&delta, &mut hunks, false)? {
                        changes.push(c);
                    }
                }
            }
            ScopeMode::Revision => {
                let rev =
                    revision.ok_or_else(|| GitError::RevisionNotFound("(none provided)".into()))?;
                let commit = self
                    .repo
                    .revparse_single(rev)
                    .and_then(|o| o.peel_to_commit())
                    .map_err(|_| GitError::RevisionNotFound(rev.to_string()))?;
                let new_tree = commit.tree()?;
                let parent = commit.parent(0).ok();
                let parent_tree = match &parent {
                    Some(p) => Some(p.tree()?),
                    None => None,
                };
                let empty;
                let base: &Tree<'_> = match &parent_tree {
                    Some(t) => t,
                    None => {
                        empty = self.empty_tree()?;
                        &empty
                    }
                };
                let diff =
                    self.repo
                        .diff_tree_to_tree(Some(base), Some(&new_tree), Some(&mut opts))?;
                let mut hunks = Self::gather_hunks(&diff)?;
                for delta in diff.deltas() {
                    if let Some(c) = self.build_change(&delta, &mut hunks, false)? {
                        changes.push(c);
                    }
                }
            }
        }

        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(changes)
    }

    /// Group hunks by file path: `(path, new-side lines, old-side lines)`.
    fn gather_hunks(diff: &git2::Diff<'_>) -> Result<Vec<FileHunks>, GitError> {
        let mut hunks: Vec<FileHunks> = Vec::new();
        let mut current: Option<String> = None;
        let mut new_lines: Vec<u32> = Vec::new();
        let mut old_lines: Vec<u32> = Vec::new();

        let mut file_cb = |_d: git2::DiffDelta<'_>, _f: f32| true;
        let mut hunk_cb = |delta: git2::DiffDelta<'_>, hunk: git2::DiffHunk| {
            let key = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if current.as_deref() != Some(key.as_str()) {
                if let Some(prev) = current.take() {
                    hunks.push((
                        prev,
                        std::mem::take(&mut new_lines),
                        std::mem::take(&mut old_lines),
                    ));
                }
                current = Some(key);
            }
            let new_count = hunk.new_lines(); // count, not start
            let old_count = hunk.old_lines();
            if new_count > 0 {
                for l in hunk.new_start()..hunk.new_start() + new_count {
                    new_lines.push(l);
                }
            }
            if old_count > 0 {
                for l in hunk.old_start()..hunk.old_start() + old_count {
                    old_lines.push(l);
                }
            }
            true
        };
        diff.foreach(&mut file_cb, None, Some(&mut hunk_cb), None)?;
        if let Some(prev) = current.take() {
            hunks.push((prev, new_lines, old_lines));
        }
        Ok(hunks)
    }

    /// Assemble one [`Change`] from a delta plus its hunk line ranges.
    ///
    /// `read_new_from_disk` is set for the worktree scope, where the "new"
    /// side of the diff is a file that may not exist as an object in the
    /// database (it is hashed on the fly by libgit2).
    fn build_change(
        &self,
        delta: &git2::DiffDelta<'_>,
        hunks: &mut Vec<FileHunks>,
        read_new_from_disk: bool,
    ) -> Result<Option<Change>, GitError> {
        let new_path = delta
            .new_file()
            .path()
            .map(|p| p.to_string_lossy().replace('\\', "/"));
        let old_path = delta
            .old_file()
            .path()
            .map(|p| p.to_string_lossy().replace('\\', "/"));
        let key = new_path
            .clone()
            .or_else(|| old_path.clone())
            .unwrap_or_default();
        let (mut nl, mut ol) = match hunks.iter().position(|(k, _, _)| *k == key) {
            Some(i) => {
                let (k, n, o) = hunks.remove(i);
                let _ = k;
                (n, o)
            }
            None => (Vec::new(), Vec::new()),
        };
        nl.sort_unstable();
        nl.dedup();
        ol.sort_unstable();
        ol.dedup();

        let binary = delta.flags().contains(git2::DiffFlags::BINARY);

        let old_content = if delta.old_file().id().is_zero() {
            None
        } else {
            self.blob_text(&delta.old_file().id())
        };
        let new_content = if delta.new_file().id().is_zero() {
            None
        } else if read_new_from_disk {
            match self.read_worktree_file(&key) {
                Ok(Some(bytes)) => {
                    if bytes.contains(&0) {
                        None
                    } else {
                        String::from_utf8(bytes).ok()
                    }
                }
                Ok(None) => None,
                Err(_) => None,
            }
        } else {
            self.blob_text(&delta.new_file().id())
        };

        let category = match delta.old_file().id().is_zero() {
            true => crate::evidence::ChangeCategory::Added,
            false => {
                if delta.new_file().id().is_zero() {
                    crate::evidence::ChangeCategory::Deleted
                } else if old_path != new_path {
                    crate::evidence::ChangeCategory::Renamed
                } else {
                    crate::evidence::ChangeCategory::Modified
                }
            }
        };
        let path = new_path
            .clone()
            .or_else(|| old_path.clone())
            .unwrap_or_default();
        let is_rename = category == crate::evidence::ChangeCategory::Renamed;
        Ok(Some(Change {
            path,
            old_path: if is_rename { old_path } else { None },
            category,
            old_content,
            new_content,
            new_lines: nl,
            old_lines: ol,
            binary,
        }))
    }

    fn blob_text(&self, id: &git2::Oid) -> Option<String> {
        if id.is_zero() {
            return None;
        }
        let blob = self.repo.find_blob(*id).ok()?;
        let bytes = blob.content();
        if bytes.contains(&0) {
            return None; // binary
        }
        String::from_utf8(bytes.to_vec()).ok()
    }

    /// Read a file's content at a specific revision.
    pub fn file_at_revision(&self, rev: &str, path: &str) -> Option<String> {
        let obj = self.repo.revparse_single(rev).ok()?;
        let commit = obj.peel_to_commit().ok()?;
        let tree = commit.tree().ok()?;
        let entry = tree.get_path(Path::new(path)).ok()?;
        let blob = self.repo.find_blob(entry.id()).ok()?;
        String::from_utf8(blob.content().to_vec()).ok()
    }

    /// Latest commits touching the repository, with the files each touched.
    ///
    /// Bounded by `limit`; the walk is deliberately shallow because history is
    /// optional context, not evidence.
    pub fn recent_commits(
        &self,
        limit: u32,
    ) -> Result<Vec<crate::evidence::HistoryCommit>, GitError> {
        let head = self
            .repo
            .head()
            .and_then(|h| h.peel_to_commit())
            .map_err(|_| GitError::RevisionNotFound("HEAD".into()))?;
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push(head.id())?;
        revwalk.set_sorting(git2::Sort::TIME)?;

        let mut out = Vec::new();
        for oid in revwalk {
            if out.len() as u32 >= limit {
                break;
            }
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            let tree = commit.tree()?;
            let parent_commit = commit.parent(0).ok();
            let parent_tree = match &parent_commit {
                Some(p) => Some(p.tree()?),
                None => None,
            };
            let mut files = Vec::new();
            let mut opts = DiffOptions::new();
            opts.context_lines(0);
            let diff =
                self.repo
                    .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))?;
            for delta in diff.deltas() {
                let p = delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .map(|p| p.to_string_lossy().replace('\\', "/"));
                if let Some(p) = p {
                    files.push(p);
                }
            }
            files.sort();
            files.dedup();
            let subject = String::from_utf8_lossy(commit.message_bytes())
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            out.push(crate::evidence::HistoryCommit {
                id: oid.to_string(),
                subject,
                files,
            });
        }
        Ok(out)
    }

    /// Read a worktree file safely: it must resolve inside the repository.
    pub fn read_worktree_file(&self, rel: &str) -> std::io::Result<Option<Vec<u8>>> {
        let joined = self.root.join(rel);
        // Reject obvious traversal even before touching the filesystem.
        if rel.split('/').any(|seg| seg == "..") {
            return Err(std::io::Error::other("path traversal rejected"));
        }
        if !joined.exists() {
            return Ok(None);
        }
        let canon = joined.canonicalize()?;
        let root_canon = self.root.canonicalize()?;
        if !canon.starts_with(&root_canon) {
            return Err(std::io::Error::other("symlink escape rejected"));
        }
        std::fs::read(canon).map(Some)
    }
}

/// Read-only helpers for constructing fixture repositories in tests.
///
/// These are the only functions in this crate that write to a repository, and
/// they are only ever pointed at throwaway directories created by tests.
#[doc(hidden)]
pub mod testkit {
    use super::*;

    /// Fixed signature so fixture commits are byte-reproducible.
    pub fn commit_time(seed: u64) -> git2::Time {
        git2::Time::new(1_600_000_000 + (seed as i64) * 60, 0)
    }

    /// Stage everything under `root` and commit with `msg`.
    pub fn commit_all(repo: &GitRepo, msg: &str, seed: u64) -> Result<String, GitError> {
        let mut index = repo.repository().index()?;
        // Add every file by walking the worktree (avoids .gitignore surprises).
        let root = repo.root().to_path_buf();
        let mut paths = Vec::new();
        collect_files(&root, &root, &mut paths)?;
        for p in &paths {
            let _ = index.add_path(Path::new(p));
        }
        index.write()?;
        let tree_id = index.write_tree()?;
        let tree = repo.repository().find_tree(tree_id)?;

        let signature = git2::Signature::new(
            "Aftermath Fixture",
            "fixture@aftermath.test",
            &commit_time(seed),
        )?;
        let author = signature.clone();
        let parents: Vec<git2::Commit<'_>> = match repo.repository().head() {
            Ok(head) => vec![head.peel_to_commit()?],
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => vec![],
            Err(e) => return Err(GitError::Git(e)),
        };
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        let oid = repo.repository().commit(
            Some("HEAD"),
            &author,
            &signature,
            msg,
            &tree,
            &parent_refs,
        )?;
        Ok(oid.to_string())
    }

    /// Stage a single path.
    pub fn stage(repo: &GitRepo, rel: &str) -> Result<(), GitError> {
        let mut index = repo.repository().index()?;
        index.add_path(Path::new(rel))?;
        index.write()?;
        Ok(())
    }

    /// Unstage everything (reset index to HEAD) without touching the worktree.
    pub fn reset_index(repo: &GitRepo) -> Result<(), GitError> {
        if let Ok(head) = repo.repository().head() {
            let commit = head.peel_to_commit()?;
            let tree = commit.tree()?;
            let mut idx = repo.repository().index()?;
            idx.read_tree(&tree)?;
            idx.write()?;
        }
        Ok(())
    }

    fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git" {
                continue;
            }
            if path.is_dir() {
                collect_files(root, &path, out)?;
            } else {
                let rel = path
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                out.push(rel);
            }
        }
        Ok(())
    }

    /// Write (or overwrite) a file inside the repository.
    pub fn write(repo: &GitRepo, rel: &str, content: &str) -> std::io::Result<()> {
        let path = repo.root().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)
    }

    /// Initialize a repository, commit an initial state, return the handle.
    pub fn init_with(dir: &Path, files: &[(&str, &str)]) -> Result<GitRepo, GitError> {
        std::fs::create_dir_all(dir)?;
        let repo = GitRepo::init(dir)?;
        for (rel, content) in files {
            write(&repo, rel, content)
                .map_err(|e| GitError::Git(git2::Error::from_str(&format!("write {rel}: {e}"))))?;
        }
        commit_all(&repo, "initial state", 0)?;
        Ok(repo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::ScopeMode;
    use tempfile::TempDir;

    fn fixture_basic() -> (TempDir, GitRepo) {
        let dir = TempDir::new().expect("tempdir");
        let repo = testkit::init_with(
            dir.path(),
            &[
                (
                    "src/a.js",
                    "function alpha() { return 1; }\nmodule.exports = { alpha };\n",
                ),
                ("src/b.js", "const { alpha } = require('./a');\nalpha();\n"),
            ],
        )
        .expect("init fixture");
        (dir, repo)
    }

    #[test]
    fn discovers_repository_and_head() {
        let (_dir, repo) = fixture_basic();
        assert!(repo.head_oid().is_some());
        assert!(!repo.is_dirty());
        let found = GitRepo::discover(repo.root()).expect("discover");
        assert_eq!(found.root(), repo.root());
    }

    #[test]
    fn worktree_scope_reports_modified_file_with_exact_lines() {
        let (_dir, repo) = fixture_basic();
        testkit::write(
            &repo,
            "src/a.js",
            "function alpha() { return 2; }\nfunction extra() {}\nmodule.exports = { alpha };\n",
        )
        .unwrap();
        let changes = repo.changes(ScopeMode::Worktree, None).expect("changes");
        assert_eq!(changes.len(), 1);
        let c = &changes[0];
        assert_eq!(c.path, "src/a.js");
        assert_eq!(c.category, crate::evidence::ChangeCategory::Modified);
        assert!(
            c.new_lines.contains(&1),
            "line 1 changed: {:?}",
            c.new_lines
        );
        assert!(c.new_lines.contains(&2), "line 2 added: {:?}", c.new_lines);
        assert!(
            !c.new_lines.contains(&3),
            "line 3 is context: {:?}",
            c.new_lines
        );
        assert!(c.old_content.is_some() && c.new_content.is_some());
    }

    #[test]
    fn staged_scope_sees_only_the_index() {
        let (_dir, repo) = fixture_basic();
        // Stage one change, leave another unstaged.
        testkit::write(
            &repo,
            "src/a.js",
            "function alpha() { return 3; }\nmodule.exports = { alpha };\n",
        )
        .unwrap();
        testkit::stage(&repo, "src/a.js").unwrap();
        testkit::write(
            &repo,
            "src/a.js",
            "function alpha() { return 4; }\nmodule.exports = { alpha };\n",
        )
        .unwrap();

        let staged = repo.changes(ScopeMode::Staged, None).expect("staged");
        assert_eq!(staged.len(), 1);
        assert!(
            staged[0]
                .new_content
                .as_deref()
                .unwrap()
                .contains("return 3"),
            "staged content is the index version"
        );

        let worktree = repo.changes(ScopeMode::Worktree, None).expect("worktree");
        assert_eq!(worktree.len(), 1);
        assert!(
            worktree[0]
                .new_content
                .as_deref()
                .unwrap()
                .contains("return 4"),
            "worktree scope sees the on-disk version"
        );
    }

    #[test]
    fn added_and_deleted_files_are_classified() {
        let (_dir, repo) = fixture_basic();
        testkit::write(&repo, "src/new.js", "function neu() {}\n").unwrap();
        std::fs::remove_file(repo.root().join("src/b.js")).unwrap();
        let changes = repo.changes(ScopeMode::Worktree, None).expect("changes");
        let added = changes.iter().find(|c| c.path == "src/new.js").unwrap();
        assert_eq!(added.category, crate::evidence::ChangeCategory::Added);
        assert!(added.old_content.is_none());
        let deleted = changes.iter().find(|c| c.path == "src/b.js").unwrap();
        assert_eq!(deleted.category, crate::evidence::ChangeCategory::Deleted);
        assert!(deleted.new_content.is_none());
        assert!(deleted.old_content.is_some());
        // Deleted file: old-side lines are recorded.
        assert!(
            !deleted.old_lines.is_empty(),
            "old lines recorded: {:?}",
            deleted.old_lines
        );
    }

    #[test]
    fn revision_scope_diffs_commit_against_parent() {
        let (_dir, repo) = fixture_basic();
        testkit::write(
            &repo,
            "src/a.js",
            "function alpha() { return 9; }\nmodule.exports = { alpha };\n",
        )
        .unwrap();
        let oid = testkit::commit_all(&repo, "change alpha", 1).unwrap();
        let changes = repo
            .changes(ScopeMode::Revision, Some(&oid))
            .expect("revision changes");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "src/a.js");
        // Worktree is now clean.
        assert!(!repo.is_dirty());
    }

    #[test]
    fn uncommitted_changes_make_repo_dirty() {
        let (_dir, repo) = fixture_basic();
        testkit::write(&repo, "src/a.js", "changed").unwrap();
        assert!(repo.is_dirty());
    }

    #[test]
    fn recent_commits_lists_files() {
        let (_dir, repo) = fixture_basic();
        testkit::write(&repo, "src/c.js", "function gamma() {}\n").unwrap();
        testkit::commit_all(&repo, "add c", 1).unwrap();
        let commits = repo.recent_commits(10).expect("history");
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "add c");
        assert_eq!(commits[0].files, vec!["src/c.js".to_string()]);
        assert!(commits[1].files.contains(&"src/a.js".to_string()));
    }

    #[test]
    fn worktree_file_reads_reject_traversal() {
        let (_dir, repo) = fixture_basic();
        assert!(repo.read_worktree_file("../secret.txt").is_err());
        assert!(repo.read_worktree_file("src/a.js").unwrap().is_some());
        assert!(repo.read_worktree_file("src/missing.js").unwrap().is_none());
    }

    #[test]
    fn discover_fails_outside_a_repo() {
        let dir = TempDir::new().unwrap();
        // A bare directory with no .git anywhere above it (temp dirs on
        // Windows usually live under the user profile, which may itself be a
        // repo — so only assert the error kind, not the message).
        match GitRepo::discover(dir.path()) {
            Ok(_) => {}
            Err(GitError::NotFound(_)) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}
