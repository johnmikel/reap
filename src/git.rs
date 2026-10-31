//! Reading git state without changing it. `--no-optional-locks` stops `git status` from
//! rewriting the index of a worktree another session may be working in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn run(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
}

pub fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = run(dir, args)?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

pub fn git_ok(dir: &Path, args: &[&str]) -> bool {
    run(dir, args).is_some_and(|o| o.status.success())
}

#[derive(Debug, Clone, Default)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    /// Short branch name, or None when detached.
    pub branch: Option<String>,
    pub locked: bool,
    /// Git already knows the folder is gone.
    pub prunable: bool,
}

/// Linked worktrees of a repository; the main checkout is left out.
pub fn worktrees(repo: &Path) -> Vec<Worktree> {
    let Some(text) = git(repo, &["worktree", "list", "--porcelain"]) else {
        return vec![];
    };
    text.split("\n\n")
        .skip(1)
        .filter_map(|block| {
            let mut wt = Worktree::default();
            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    wt.path = PathBuf::from(p);
                } else if let Some(h) = line.strip_prefix("HEAD ") {
                    wt.head = h.to_string();
                } else if let Some(b) = line.strip_prefix("branch ") {
                    wt.branch = Some(b.trim_start_matches("refs/heads/").to_string());
                } else if line.starts_with("locked") {
                    wt.locked = true;
                } else if line.starts_with("prunable") {
                    wt.prunable = true;
                }
            }
            (!wt.path.as_os_str().is_empty()).then_some(wt)
        })
        .collect()
}

/// What "merged" means for this repository: its remote's default branch when it has a
/// remote (the local main can hold another session's unpushed commits), else local main.
pub fn base(repo: &Path) -> Option<String> {
    if let Some(r) = git(repo, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"]) {
        return Some(r.trim_start_matches("refs/remotes/").to_string());
    }
    let has_remote = git(repo, &["remote"]).is_some_and(|r| !r.is_empty());
    let candidates: &[&str] = if has_remote {
        &["origin/main", "origin/master"]
    } else {
        &["main", "master"]
    };
    candidates
        .iter()
        .find(|b| git_ok(repo, &["rev-parse", "--verify", "--quiet", &format!("{b}^{{commit}}")]))
        .map(|b| b.to_string())
}

pub fn is_merged(repo: &Path, commit: &str, base: &str) -> bool {
    git_ok(repo, &["merge-base", "--is-ancestor", commit, base])
}

/// Commits on HEAD that no remote branch has.
pub fn unpushed(wt: &Path) -> Option<u32> {
    git(wt, &["rev-list", "--count", "HEAD", "--not", "--remotes"])?
        .parse()
        .ok()
}

pub fn is_dirty(wt: &Path) -> Option<bool> {
    git(wt, &["status", "--porcelain"]).map(|s| !s.is_empty())
}

pub fn mtime(p: &Path) -> Option<u64> {
    let t = fs::metadata(p).ok()?.modified().ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Last time git did anything in a worktree: its HEAD, index or reflog changing.
pub fn last_activity(wt: &Path) -> Option<u64> {
    let dir = git(wt, &["rev-parse", "--absolute-git-dir"])?;
    let dir = PathBuf::from(dir);
    ["HEAD", "index", "logs/HEAD"]
        .iter()
        .filter_map(|f| mtime(&dir.join(f)))
        .max()
}

/// Last time anyone worked in a main checkout: its last commit or index change.
pub fn repo_activity(repo: &Path) -> Option<u64> {
    let commit = git(repo, &["log", "-1", "--format=%ct"]).and_then(|s| s.parse().ok());
    let dir = PathBuf::from(git(repo, &["rev-parse", "--absolute-git-dir"])?);
    [commit, mtime(&dir.join("index")), mtime(&dir.join("HEAD"))]
        .into_iter()
        .flatten()
        .max()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A scratch folder that cleans up after itself.
    pub struct Tmp(pub PathBuf);
    impl Tmp {
        pub fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!("reap-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            Tmp(d.canonicalize().unwrap())
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub fn sh(dir: &Path, cmd: &str) {
        let ok = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(ok.status.success(), "{cmd}: {}", String::from_utf8_lossy(&ok.stderr));
    }

    /// origin (bare) and a clone with a pushed main.
    pub fn repo_with_remote(t: &Tmp) -> PathBuf {
        sh(
            &t.0,
            "git init -q --bare -b main origin.git && git clone -q origin.git app 2>/dev/null",
        );
        let app = t.0.join("app");
        sh(
            &app,
            "echo hi > a.txt && git add a.txt && git commit -qm one && git push -q origin main",
        );
        app
    }

    #[test]
    fn reads_worktrees_base_and_state() {
        let t = Tmp::new("git");
        let app = repo_with_remote(&t);
        sh(&app, "git worktree add -q .worktrees/fix/done -b fix/done && git worktree add -q .worktrees/feat/wip -b feat/wip");
        sh(
            &app.join(".worktrees/feat/wip"),
            "echo x > b.txt && git add b.txt && git commit -qm wip",
        );
        // git lists worktrees by path, so feat/ comes before fix/.
        let wts = worktrees(&app);
        assert_eq!(wts.len(), 2);
        let wts = [wts[1].clone(), wts[0].clone()];
        assert_eq!(wts[0].branch.as_deref(), Some("fix/done"));
        let base = base(&app).unwrap();
        assert_eq!(base, "origin/main");
        assert!(is_merged(&app, &wts[0].head, &base));
        assert!(!is_merged(&app, &wts[1].head, &base));
        assert_eq!(unpushed(&wts[1].path), Some(1));
        assert_eq!(is_dirty(&wts[0].path), Some(false));
        fs::write(wts[0].path.join("a.txt"), "changed").unwrap();
        assert_eq!(is_dirty(&wts[0].path), Some(true));
        assert!(last_activity(&wts[1].path).unwrap() + 60 > now());
    }
}
