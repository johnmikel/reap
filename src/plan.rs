//! Deciding what is safe to clear. A worktree qualifies only when it holds nothing that
//! exists nowhere else: no uncommitted changes, every commit merged (or pushed), no env
//! file of its own, and no git activity for a few days, since another session may be in it.
//! Build caches qualify when their project has been idle for weeks; they rebuild on demand.

use crate::config;
use crate::git::{self, Worktree};
use crate::size::Size;
use std::fs;
use std::path::{Path, PathBuf};

pub const WORKTREE_IDLE_DAYS: u64 = 3;
const DAY: u64 = 86_400;
const ENV_NAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development",
    ".env.development.local",
    ".env.test",
    ".env.test.local",
    ".dev.vars",
    ".env.production",
    ".env.production.local",
];
const SKIP: &[&str] = &[
    "node_modules",
    "Library",
    "target",
    "dist",
    "build",
    "Pods",
    "DerivedData",
    "vendor",
];
const MARKERS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "Podfile",
    "pubspec.yaml",
    "go.mod",
];

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Run the close hook configured for the repository, with its placeholders filled in.
    Hook { command: String, repo: PathBuf },
    /// Move the worktree folder to the Trash, then prune git's record of it.
    TrashWorktree { repo: PathBuf },
    /// Move the folder to the Trash.
    Trash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Worktree,
    Cache,
    Xcode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Keep {
    Uncommitted,
    Unmerged,
    Recent,
    EnvFile,
    Locked,
    Busy,
    Unreadable,
}

impl Keep {
    pub fn describe(self) -> &'static str {
        match self {
            Keep::Uncommitted => "uncommitted changes",
            Keep::Unmerged => "commits that exist only there",
            Keep::Recent => "recent git activity",
            Keep::EnvFile => "an env file of their own",
            Keep::Locked => "a lock",
            Keep::Busy => "a program running inside (an agent, a server, a shell)",
            Keep::Unreadable => "a state git couldn't read",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub path: PathBuf,
    pub group: Group,
    /// Where it lives, for display: repo and branch, or project and folder.
    pub label: String,
    /// Why it is safe, or why it is kept.
    pub detail: String,
    pub action: Option<Action>,
    pub keep: Option<Keep>,
    pub size: Size,
}

fn days_ago(t: u64, now: u64) -> String {
    let d = now.saturating_sub(t) / DAY;
    match d {
        0 => format!("{} h", now.saturating_sub(t) / 3600),
        _ => format!("{d} d"),
    }
}

fn name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Main checkouts (folders with a .git directory) up to three levels under each root.
pub fn find_repos(roots: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if dir.join(".git").is_dir() {
            out.push(dir.to_path_buf());
        }
        if depth >= 3 {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with('.') && !SKIP.contains(&n.as_str()) && e.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&e.path(), depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for r in roots {
        walk(r, 0, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// Folders running programs are working in, so an agent mid-task, a dev server or a shell
/// parked in a worktree keeps it. reap's own process doesn't count.
pub fn working_dirs() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let me = std::process::id().to_string();
        fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()) && n != me)
            })
            .filter_map(|e| fs::read_link(e.path().join("cwd")).ok())
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Ok(out) = std::process::Command::new("lsof")
            .args(["-a", "-d", "cwd", "-F", "pn", "-w"])
            .stderr(std::process::Stdio::null())
            .output()
        else {
            return vec![];
        };
        let me = format!("p{}", std::process::id());
        let mut mine = false;
        let mut dirs = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.starts_with('p') {
                mine = line == me;
            } else if let (false, Some(path)) = (mine, line.strip_prefix('n')) {
                dirs.push(PathBuf::from(path));
            }
        }
        dirs
    }
}

pub fn is_busy(path: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|d| d.starts_with(path))
}

/// A worktree something is running in.
pub fn busy(repo: &Path, wt: &Worktree) -> Item {
    let branch = wt
        .branch
        .clone()
        .unwrap_or_else(|| format!("detached {}", &wt.head[..wt.head.len().min(8)]));
    Item {
        path: wt.path.clone(),
        group: Group::Worktree,
        label: format!("{} · {branch}", name(repo)),
        detail: "a program is running in it".into(),
        action: None,
        keep: Some(Keep::Busy),
        size: Size::default(),
    }
}

/// An env file in the worktree that the main checkout doesn't have, byte for byte.
fn own_env_file(repo: &Path, wt: &Path) -> Option<String> {
    fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_file() && ENV_NAMES.contains(&n.as_str()) {
                found.push(e.path());
            } else if ft.is_dir() && depth < 3 && !n.starts_with('.') && !SKIP.contains(&n.as_str()) {
                walk(&e.path(), depth + 1, found);
            }
        }
    }
    let mut found = Vec::new();
    walk(wt, 0, &mut found);
    found.into_iter().find_map(|f| {
        let rel = f.strip_prefix(wt).ok()?.to_path_buf();
        let same = matches!((fs::read(&f), fs::read(repo.join(&rel))), (Ok(a), Ok(b)) if a == b);
        (!same).then(|| rel.display().to_string())
    })
}

/// `hook` is the close hook configured for this repository, if any. Repositories with one
/// manage their own worktrees, so reap hands over only those merged into the base; the
/// hook's own checks get the final say when it runs.
pub fn classify_worktree(
    repo: &Path,
    base: Option<&str>,
    wt: &Worktree,
    now: u64,
    idle_days: u64,
    hook: Option<&str>,
) -> Item {
    let short = &wt.head[..wt.head.len().min(8)];
    let branch = wt.branch.clone().unwrap_or_else(|| format!("detached {short}"));
    let mut item = Item {
        path: wt.path.clone(),
        group: Group::Worktree,
        label: format!("{} · {branch}", name(repo)),
        detail: String::new(),
        action: None,
        keep: None,
        size: Size::default(),
    };
    let mut keep = |k: Keep, why: String| {
        item.keep = Some(k);
        item.detail = why;
        item.clone()
    };
    if wt.locked {
        return keep(Keep::Locked, "locked".into());
    }
    match git::is_dirty(&wt.path) {
        None => return keep(Keep::Unreadable, "git couldn't read it".into()),
        Some(true) => return keep(Keep::Uncommitted, "uncommitted changes".into()),
        Some(false) => {}
    }
    let last = git::last_activity(&wt.path).unwrap_or(now);
    if now.saturating_sub(last) < idle_days * DAY {
        return keep(Keep::Recent, format!("git active {} ago", days_ago(last, now)));
    }
    let merged = base.filter(|b| git::is_merged(repo, &wt.head, b));
    if merged.is_none() {
        let pushed = hook.is_none() && git::unpushed(&wt.path) == Some(0) && base.is_some_and(|b| b.contains('/'));
        if !pushed {
            let n = git::unpushed(&wt.path).unwrap_or(0);
            let why = match base {
                Some(b) if n > 0 => format!("{n} commit{} not in {b}", if n == 1 { "" } else { "s" }),
                Some(b) => format!("not merged into {b}"),
                None => "no main branch to compare with".into(),
            };
            return keep(Keep::Unmerged, why);
        }
    }
    if let Some(rel) = own_env_file(repo, &wt.path) {
        return keep(Keep::EnvFile, format!("own {rel}"));
    }
    let idle = format!("idle {}", days_ago(last, now));
    let (action, why) = match (hook, merged) {
        (Some(hook), Some(b)) => (
            Action::Hook {
                command: config::render(hook, repo, &wt.path, wt.branch.as_deref().unwrap_or(short), b),
                repo: repo.to_path_buf(),
            },
            format!("merged into {b} · {idle} · closes with your hook"),
        ),
        (_, Some(b)) => (
            Action::TrashWorktree {
                repo: repo.to_path_buf(),
            },
            format!("merged into {b} · {idle}"),
        ),
        (_, None) => (
            Action::TrashWorktree {
                repo: repo.to_path_buf(),
            },
            format!("every commit pushed · {idle}"),
        ),
    };
    item.action = Some(action);
    item.detail = why;
    item
}

fn is_cache(dir: &Path, n: &str) -> bool {
    let parent = dir.parent().unwrap_or(dir);
    match n {
        "node_modules" | ".next" | ".turbo" | ".expo" | ".svelte-kit" | ".nuxt" | ".parcel-cache" | ".gradle" => true,
        "target" => parent.join("Cargo.toml").is_file(),
        "Pods" => parent.join("Podfile").is_file(),
        ".venv" | "venv" => dir.join("pyvenv.cfg").is_file(),
        _ => false,
    }
}

/// Rebuildable folders in a project, without descending into them.
fn caches_in(project: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let n = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            if is_cache(&p, &n) {
                out.push(p);
            } else if depth < 4 && !n.starts_with('.') && !["dist", "build", "Library"].contains(&n.as_str()) {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(project, 0, &mut out);
    out
}

/// Newest file change in a folder that isn't a git repo, ignoring rebuildable folders.
fn folder_activity(dir: &Path) -> Option<u64> {
    fn walk(dir: &Path, depth: usize, newest: &mut u64) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_file() {
                *newest = (*newest).max(git::mtime(&p).unwrap_or(0));
            } else if ft.is_dir() && depth < 4 && !is_cache(&p, &n) && n != ".git" {
                walk(&p, depth + 1, newest);
            }
        }
    }
    let mut newest = 0;
    walk(dir, 0, &mut newest);
    (newest > 0).then_some(newest)
}

/// Project folders: every git checkout, plus folders with a build file that aren't inside one.
pub fn find_projects(roots: &[PathBuf], repos: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, repos: &[PathBuf], out: &mut Vec<PathBuf>) {
        if repos.iter().any(|r| dir.starts_with(r)) {
            return;
        }
        if MARKERS.iter().any(|m| dir.join(m).is_file()) {
            out.push(dir.to_path_buf());
            return;
        }
        if depth >= 3 {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with('.') && !SKIP.contains(&n.as_str()) && e.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&e.path(), depth + 1, repos, out);
            }
        }
    }
    let mut out = repos.to_vec();
    for r in roots {
        walk(r, 0, repos, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

pub fn idle_caches(projects: &[PathBuf], repos: &[PathBuf], days: u64, now: u64) -> Vec<Item> {
    let mut out = Vec::new();
    for p in projects {
        let last = if repos.contains(p) {
            git::repo_activity(p)
        } else {
            folder_activity(p)
        };
        let Some(last) = last else { continue };
        if now.saturating_sub(last) < days * DAY {
            continue;
        }
        for c in caches_in(p) {
            let rel = c.strip_prefix(p).unwrap_or(&c).display().to_string();
            out.push(Item {
                path: c,
                group: Group::Cache,
                label: format!("{}/{rel}", name(p)),
                detail: format!("project idle {}", days_ago(last, now)),
                action: Some(Action::Trash),
                keep: None,
                size: Size::default(),
            });
        }
    }
    out
}

/// Xcode's per-project build folders that haven't been touched in `days`.
pub fn idle_derived_data(home: &Path, days: u64, now: u64) -> Vec<Item> {
    let dd = home.join("Library/Developer/Xcode/DerivedData");
    let Ok(rd) = fs::read_dir(&dd) else { return vec![] };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let n = e.file_name().to_string_lossy().to_string();
        if !e.file_type().is_ok_and(|t| t.is_dir()) || n.ends_with(".noindex") {
            continue;
        }
        let children = fs::read_dir(&p)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|c| git::mtime(&c.path()));
        let last = children.chain(git::mtime(&p)).max().unwrap_or(now);
        if now.saturating_sub(last) >= days * DAY {
            let project = n.rsplit_once('-').map_or(n.as_str(), |(a, _)| a).to_string();
            out.push(Item {
                path: p,
                group: Group::Xcode,
                label: format!("Xcode · {project}"),
                detail: format!("last build {} ago", days_ago(last, now)),
                action: Some(Action::Trash),
                keep: None,
                size: Size::default(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{repo_with_remote, sh, Tmp};

    fn set_age(p: &Path, days: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(days * DAY);
        fs::File::options()
            .append(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// Make a worktree look untouched by git for `days`.
    fn age(wt: &Path, days: u64) {
        let dir = PathBuf::from(git::git(wt, &["rev-parse", "--absolute-git-dir"]).unwrap());
        for f in ["HEAD", "index", "logs/HEAD"] {
            if dir.join(f).exists() {
                set_age(&dir.join(f), days);
            }
        }
    }

    #[test]
    fn only_finished_idle_worktrees_are_safe() {
        let t = Tmp::new("plan");
        let app = repo_with_remote(&t);
        // Like real projects, env files are ignored, so only the env check can catch them.
        sh(
            &app,
            "printf '.env*\\n' > .gitignore && git add .gitignore && git commit -qm ignore && git push -q origin main",
        );
        for b in ["fix/merged", "feat/local", "feat/dirty", "feat/fresh", "feat/env"] {
            sh(&app, &format!("git worktree add -q .worktrees/{b} -b {b}"));
        }
        let w = |b: &str| app.join(".worktrees").join(b);
        sh(
            &w("feat/local"),
            "echo x > b.txt && git add b.txt && git commit -qm local",
        );
        fs::write(w("feat/dirty").join("a.txt"), "edit").unwrap();
        fs::write(w("feat/env").join(".env.local"), "KEY=only-here").unwrap();
        for b in ["fix/merged", "feat/local", "feat/dirty", "feat/env"] {
            age(&w(b), 10);
        }
        let base = git::base(&app);
        let now = git::now();
        let by: std::collections::HashMap<String, Item> = git::worktrees(&app)
            .iter()
            .map(|wt| {
                (
                    wt.branch.clone().unwrap(),
                    classify_worktree(&app, base.as_deref(), wt, now, WORKTREE_IDLE_DAYS, None),
                )
            })
            .collect();
        assert_eq!(
            by["fix/merged"].action,
            Some(Action::TrashWorktree { repo: app.clone() }),
            "{:?}",
            by["fix/merged"]
        );
        assert_eq!(by["feat/local"].keep, Some(Keep::Unmerged));
        assert_eq!(by["feat/dirty"].keep, Some(Keep::Uncommitted));
        assert_eq!(by["feat/fresh"].keep, Some(Keep::Recent));
        assert_eq!(by["feat/env"].keep, Some(Keep::EnvFile));
    }

    #[test]
    fn a_close_hook_takes_only_merged_worktrees_and_gets_its_placeholders() {
        let t = Tmp::new("hook");
        let app = repo_with_remote(&t);
        sh(&app, "git worktree add -q .worktrees/fix/tidy-up -b fix/tidy-up && git worktree add -q .worktrees/feat/pushed -b feat/pushed");
        sh(
            &app.join(".worktrees/feat/pushed"),
            "echo x > b.txt && git add b.txt && git commit -qm wip && git push -q origin feat/pushed",
        );
        for b in ["fix/tidy-up", "feat/pushed"] {
            age(&app.join(".worktrees").join(b), 10);
        }
        let base = git::base(&app);
        let hook = Some("close-it {repo} {branch} {base}");
        let by: std::collections::HashMap<String, Item> = git::worktrees(&app)
            .iter()
            .map(|wt| {
                (
                    wt.branch.clone().unwrap(),
                    classify_worktree(&app, base.as_deref(), wt, git::now(), WORKTREE_IDLE_DAYS, hook),
                )
            })
            .collect();
        let expected = format!("close-it '{}' 'fix/tidy-up' 'origin/main'", app.display());
        assert_eq!(
            by["fix/tidy-up"].action,
            Some(Action::Hook {
                command: expected,
                repo: app.clone()
            })
        );
        // Pushed but not merged would go to the Trash without a hook; a hook's repository keeps it.
        assert_eq!(by["feat/pushed"].keep, Some(Keep::Unmerged));
        let no_hook = classify_worktree(
            &app,
            base.as_deref(),
            &git::worktrees(&app)[0],
            git::now(),
            WORKTREE_IDLE_DAYS,
            None,
        );
        assert_eq!(
            no_hook.action,
            Some(Action::TrashWorktree { repo: app.clone() }),
            "{no_hook:?}"
        );
    }

    #[test]
    fn a_program_running_in_a_worktree_marks_it_busy() {
        let t = Tmp::new("busy");
        let parked = t.0.join("parked");
        fs::create_dir_all(&parked).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&parked)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let dirs = working_dirs();
        let _ = child.kill();
        let _ = child.wait();
        assert!(is_busy(&parked, &dirs), "sleep's folder should show up as in use");
        assert!(!is_busy(&t.0.join("idle"), &dirs));
    }

    #[test]
    fn caches_count_only_in_idle_projects() {
        let t = Tmp::new("caches");
        for p in ["old", "new"] {
            fs::create_dir_all(t.0.join(p).join("node_modules/x")).unwrap();
            fs::write(t.0.join(p).join("package.json"), "{}").unwrap();
            fs::create_dir_all(t.0.join(p).join("target")).unwrap();
        }
        fs::write(t.0.join("old/Cargo.toml"), "").unwrap();
        for f in ["old/package.json", "old/Cargo.toml"] {
            set_age(&t.0.join(f), 90);
        }
        let roots = vec![t.0.clone()];
        let projects = find_projects(&roots, &[]);
        assert_eq!(projects.len(), 2);
        let items = idle_caches(&projects, &[], 30, git::now());
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels.len(), 2, "{labels:?}");
        assert!(
            labels.contains(&"old/node_modules") && labels.contains(&"old/target"),
            "{labels:?}"
        );
    }
}
