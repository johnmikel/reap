//! reap: clear git worktrees and build caches that hold nothing you'd lose, and say how much
//! space that really gives back.

use reap::config::Config;
use reap::git::{self, Worktree};
use reap::plan::{self, Action, Group, Item, Keep};
use reap::size::{self, human, Size};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const HELP: &str = "\
reap: clear git worktrees and build caches that hold nothing you'd lose.

usage:
  reap [ROOT...] [--all] [--days N] [--idle N] [--xcode]
                                         report only: what is safe to clear and how much space it really
                                         frees. ROOT defaults to the config's roots, else this folder.
                                         Build folders count after N idle days (default 30); finished
                                         worktrees after --idle days without git activity (default 3).
                                         --xcode also checks Xcode's DerivedData.
  reap clean [same options] [--dry-run] [--yes]
                                         list again, ask, then clear: worktrees in repositories with a
                                         close hook go through it; everything else moves to a dated
                                         folder in the Trash
  reap size PATH...                      what deleting each folder would really give back, next to
                                         what du says
  reap --version

A worktree is safe only with no uncommitted changes, every commit merged into the remote's main
(or pushed), no env file of its own, no program running inside it, and no recent git activity.
Sizes count shared (cloned) blocks once, and only when every copy goes, so they are what
emptying the Trash gives back.

Settings live in ~/.config/reap/config:
  root = ~/code
  xcode = true
  close ~/work = ~/work/bin/close-worktree {repo} {branch} {base}
";

const XCODE_DAYS: u64 = 14;
const SHOWN: usize = 12;

struct Args {
    roots: Vec<PathBuf>,
    /// Also check Xcode's build data, which lives outside any project.
    xcode: bool,
    all: bool,
    days: u64,
    /// Days without git activity before a finished worktree counts as safe.
    idle: u64,
    yes: bool,
    dry_run: bool,
}

fn parse(rest: &[String], cfg: &Config) -> Result<Args, String> {
    let mut a = Args {
        roots: vec![],
        xcode: cfg.xcode,
        all: false,
        days: 30,
        idle: plan::WORKTREE_IDLE_DAYS,
        yes: false,
        dry_run: false,
    };
    let mut it = rest.iter();
    while let Some(s) = it.next() {
        match s.as_str() {
            "--all" | "-a" => a.all = true,
            "--xcode" => a.xcode = true,
            "--yes" | "-y" => a.yes = true,
            "--dry-run" | "-n" => a.dry_run = true,
            "--days" => a.days = it.next().and_then(|d| d.parse().ok()).ok_or("--days needs a number")?,
            "--idle" => {
                a.idle = it
                    .next()
                    .and_then(|d| d.parse().ok())
                    .ok_or("--idle needs a number of days")?
            }
            f if f.starts_with('-') => return Err(format!("unknown option {f}; see reap help")),
            p => a
                .roots
                .push(PathBuf::from(p).canonicalize().map_err(|e| format!("{p}: {e}"))?),
        }
    }
    if a.roots.is_empty() {
        a.roots = cfg.roots.iter().filter_map(|r| r.canonicalize().ok()).collect();
    }
    if a.roots.is_empty() {
        a.roots.push(std::env::current_dir().map_err(|e| e.to_string())?);
    }
    Ok(a)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

struct Style(bool);

impl Style {
    fn new() -> Style {
        Style(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }
    fn paint(&self, code: &str, s: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn cmd(&self, s: &str) -> String {
        self.paint("36", s)
    }
    fn ok(&self, s: &str) -> String {
        self.paint("32", s)
    }
    fn warn(&self, s: &str) -> String {
        self.paint("33", s)
    }
}

/// Run `f` over `items` on all cores, keeping order.
fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(12);
    let next = AtomicUsize::new(0);
    let out: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else { break };
                *out[i].lock().unwrap() = Some(f(item));
            });
        }
    });
    out.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect()
}

struct Scan {
    safe: Vec<Item>,
    kept: Vec<Item>,
    /// What clearing everything safe frees, counting data the items share with each other.
    total: Size,
    group_totals: BTreeMap<Group, Size>,
    repos: usize,
    worktrees: usize,
    caches: usize,
    secs: f64,
}

fn scan(a: &Args, cfg: &Config) -> Scan {
    let t0 = Instant::now();
    let now = git::now();
    let repos = plan::find_repos(&a.roots);
    let mut wts: Vec<(PathBuf, Option<String>, Option<String>, Worktree)> = Vec::new();
    for r in &repos {
        let base = git::base(r);
        let hook = cfg.hook_for(r).map(|h| h.command.clone());
        for wt in git::worktrees(r) {
            if !wt.prunable && wt.path.is_dir() {
                wts.push((r.clone(), base.clone(), hook.clone(), wt));
            }
        }
    }
    let worktrees = wts.len();
    let in_use = plan::working_dirs();
    let items = parallel_map(&wts, |(r, b, h, wt)| {
        if plan::is_busy(&wt.path, &in_use) {
            plan::busy(r, wt)
        } else {
            plan::classify_worktree(r, b.as_deref(), wt, now, a.idle, h.as_deref())
        }
    });
    let (mut safe, kept): (Vec<Item>, Vec<Item>) = items.into_iter().partition(|i| i.action.is_some());
    let projects = plan::find_projects(&a.roots, &repos);
    let caches = plan::idle_caches(&projects, &repos, a.days, now);
    let n_caches = caches.len();
    safe.extend(caches);
    if a.xcode {
        safe.extend(plan::idle_derived_data(&home(), a.days.min(XCODE_DAYS), now));
    }
    // A repository nested in another project shows up under both; keep one entry per folder.
    safe.sort_by(|x, y| x.path.cmp(&y.path));
    safe.dedup_by(|x, y| x.path == y.path);
    let paths: Vec<PathBuf> = safe.iter().map(|i| i.path.clone()).collect();
    let measured = size::measure_all(&paths);
    for (item, size) in safe.iter_mut().zip(measured.sizes()) {
        item.size = size;
    }
    let total = measured.total(0..safe.len());
    let mut group_totals = BTreeMap::new();
    for g in [Group::Worktree, Group::Cache, Group::Xcode] {
        group_totals.insert(g, measured.total((0..safe.len()).filter(|&i| safe[i].group == g)));
    }
    safe.retain(|i| i.size.du > 0 || i.group == Group::Worktree);
    safe.sort_by_key(|i| (i.group, std::cmp::Reverse(i.size.freed)));
    Scan {
        safe,
        kept,
        total,
        group_totals,
        repos: repos.len(),
        worktrees,
        caches: n_caches,
        secs: t0.elapsed().as_secs_f64(),
    }
}

fn total(items: &[&Item]) -> Size {
    items.iter().fold(Size::default(), |mut t, i| {
        t.freed += i.size.freed;
        t.du += i.size.du;
        t
    })
}

fn group_title(g: Group) -> &'static str {
    match g {
        Group::Worktree => "Worktrees with nothing of their own",
        Group::Cache => "Build folders in idle projects",
        Group::Xcode => "Xcode build data",
    }
}

fn print_report(s: &Scan, a: &Args, st: &Style) {
    let all: Vec<&Item> = s.safe.iter().collect();
    let t = s.total;
    let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    println!(
        "{} {}, {} and {} checked in {}",
        st.bold("reap ·"),
        count(s.repos, "repo", "repos"),
        count(s.worktrees, "worktree", "worktrees"),
        count(s.caches, "build folder", "build folders"),
        if s.secs < 1.0 {
            "under a second".to_string()
        } else {
            format!("{:.0} s", s.secs)
        }
    );
    if s.safe.is_empty() {
        println!("\nNothing is safe to clear right now.");
    } else {
        println!(
            "\n{} {}  {}",
            st.bold("Safe to clear:"),
            st.bold(&human(t.freed)),
            st.dim(&format!("du would say {}; shared blocks count once", human(t.du)))
        );
        let width = all
            .iter()
            .take(if a.all { usize::MAX } else { SHOWN * 3 })
            .map(|i| i.label.chars().count())
            .max()
            .unwrap_or(20)
            .clamp(20, 64);
        let mut groups: BTreeMap<Group, Vec<&Item>> = BTreeMap::new();
        for i in &s.safe {
            groups.entry(i.group).or_default().push(i);
        }
        for (g, items) in &groups {
            let freed = s.group_totals.get(g).map_or_else(|| total(items).freed, |t| t.freed);
            println!("\n  {}  {}", st.bold(group_title(*g)), st.dim(&human(freed)));
            let shown = if a.all { items.len() } else { SHOWN.min(items.len()) };
            for i in &items[..shown] {
                let label: String = i.label.chars().take(width).collect();
                println!("    {label:<width$}  {:>7}  {}", human(i.size.freed), st.dim(&i.detail));
            }
            if shown < items.len() {
                let rest: Vec<&Item> = items[shown..].to_vec();
                println!(
                    "    {}",
                    st.dim(&format!(
                        "and {} more, {} (reap --all lists them)",
                        rest.len(),
                        human(total(&rest).freed)
                    ))
                );
            }
            // Clones shared between these items are freed only when they all go.
            let shared = freed.saturating_sub(total(items).freed);
            if shared >= 1_000_000 {
                println!(
                    "    {}",
                    st.dim(&format!(
                        "+ {} of cloned data they share with each other, freed when they go together",
                        human(shared)
                    ))
                );
            }
        }
    }
    if !s.kept.is_empty() {
        let mut counts: BTreeMap<Keep, usize> = BTreeMap::new();
        for i in &s.kept {
            *counts.entry(i.keep.unwrap()).or_default() += 1;
        }
        println!(
            "\n{} {} worktree{}",
            st.bold("Kept:"),
            s.kept.len(),
            if s.kept.len() == 1 { "" } else { "s" }
        );
        for (k, n) in &counts {
            let why = if *k == Keep::Recent {
                format!("git activity in the last {} days", a.idle)
            } else {
                k.describe().to_string()
            };
            println!("    {n:>3}  {why}");
        }
        if a.all {
            let width = s
                .kept
                .iter()
                .map(|i| i.label.chars().count())
                .max()
                .unwrap_or(20)
                .clamp(20, 64);
            for i in &s.kept {
                let label: String = i.label.chars().take(width).collect();
                println!("    {label:<width$}  {}", st.dim(&i.detail));
            }
        }
    }
}

fn cmd_report(a: &Args, cfg: &Config) -> Result<(), String> {
    let st = Style::new();
    let s = scan(a, cfg);
    print_report(&s, a, &st);
    if !s.safe.is_empty() {
        println!(
            "\n{} {}",
            st.bold("Next:"),
            st.cmd("reap clean   lists them again and asks before touching anything")
        );
    }
    Ok(())
}

fn trash_name(i: &Item) -> String {
    i.label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Where cleared folders go: the Trash on macOS, the XDG trash elsewhere. REAP_TRASH
/// points tests at a scratch folder instead.
fn trash_root() -> PathBuf {
    if let Some(t) = std::env::var_os("REAP_TRASH") {
        return PathBuf::from(t);
    }
    if cfg!(target_os = "macos") {
        home().join(".Trash")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("Trash/files")
    }
}

fn cmd_clean(a: &Args, cfg: &Config) -> Result<(), String> {
    let st = Style::new();
    let s = scan(a, cfg);
    print_report(&s, a, &st);
    if s.safe.is_empty() || a.dry_run {
        return Ok(());
    }
    let closes = s
        .safe
        .iter()
        .filter(|i| matches!(i.action, Some(Action::Hook { .. })))
        .count();
    if !a.yes {
        let how = if closes > 0 {
            format!("{closes} worktrees close through your close hook; the rest go to the Trash")
        } else {
            "they go to the Trash".to_string()
        };
        print!("\nClear these {} items? {how}. [y/N] ", s.safe.len());
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing changed.");
            return Ok(());
        }
    }
    let stamp = Command::new("date")
        .arg("+%Y-%m-%d %H.%M")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let trash = trash_root().join(format!("reap {stamp}"));
    let (mut freed_now, mut in_trash, mut failed) = (0u64, 0u64, 0usize);
    let mut warnings: Vec<String> = Vec::new();
    // Something may have started in a worktree while the list was on screen.
    let in_use = plan::working_dirs();
    for i in &s.safe {
        let worktree = matches!(i.action, Some(Action::Hook { .. } | Action::TrashWorktree { .. }));
        let result: Result<(), String> = match &i.action {
            _ if worktree && plan::is_busy(&i.path, &in_use) => {
                Err("a program is running in it now; left alone".into())
            }
            Some(Action::Hook { command, repo }) => {
                let out = Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .current_dir(repo)
                    .output()
                    .map_err(|e| e.to_string())?;
                let said = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if out.status.success() {
                    freed_now += i.size.freed;
                    Ok(())
                } else if !i.path.exists() {
                    // The hook removed the worktree, then failed a later step (such as deleting
                    // a branch git considers unmerged). The space is back either way.
                    freed_now += i.size.freed;
                    warnings.push(format!("{}: {said}", i.label));
                    Ok(())
                } else {
                    Err(if said.is_empty() {
                        format!("the close hook failed ({})", out.status)
                    } else {
                        said
                    })
                }
            }
            Some(Action::TrashWorktree { repo }) => {
                if git::is_dirty(&i.path) != Some(false) {
                    Err("changed since the scan; left alone".into())
                } else {
                    move_to(&i.path, &trash, &trash_name(i)).map(|()| {
                        git::git(repo, &["worktree", "prune"]);
                        in_trash += i.size.freed;
                    })
                }
            }
            Some(Action::Trash) => move_to(&i.path, &trash, &trash_name(i)).map(|()| in_trash += i.size.freed),
            None => Ok(()),
        };
        match result {
            Ok(()) => println!("{} {}", st.ok("✓"), i.label),
            Err(e) => {
                failed += 1;
                println!("{} {}: {e}", st.warn("!"), i.label);
            }
        }
    }
    println!();
    if freed_now > 0 {
        println!("Freed {} now by closing worktrees.", human(freed_now));
    }
    if in_trash > 0 {
        println!(
            "{} is in the Trash under \"reap {stamp}\". Empty the Trash to get it back.",
            human(in_trash)
        );
    }
    if failed > 0 {
        println!("{failed} item{} left as they were.", if failed == 1 { "" } else { "s" });
    }
    if !warnings.is_empty() {
        println!(
            "{} worktree{} removed, but the close hook then reported a problem:",
            warnings.len(),
            if warnings.len() == 1 { " was" } else { "s were" }
        );
        for w in &warnings {
            println!("    {}", st.dim(w));
        }
    }
    Ok(())
}

/// `reap size PATH...`: what deleting each folder would really give back, next to du.
fn cmd_size(rest: &[String]) -> Result<(), String> {
    let st = Style::new();
    if rest.is_empty() {
        return Err("name one or more folders, e.g. reap size ~/Library/Developer node_modules".into());
    }
    let paths: Vec<PathBuf> = rest
        .iter()
        .map(|p| PathBuf::from(p).canonicalize().map_err(|e| format!("{p}: {e}")))
        .collect::<Result<_, _>>()?;
    let measured = size::measure_all(&paths);
    let width = rest.iter().map(|p| p.chars().count()).max().unwrap_or(10).clamp(10, 60);
    for (label, s) in rest.iter().zip(measured.sizes()) {
        println!(
            "{label:<width$}  {:>8}  {}",
            human(s.freed),
            st.dim(&format!("du says {}", human(s.du)))
        );
    }
    if paths.len() > 1 {
        let t = measured.total(0..paths.len());
        println!(
            "{:<width$}  {:>8}  {}",
            st.bold("together"),
            st.bold(&human(t.freed)),
            st.dim(&format!("du says {}", human(t.du)))
        );
    }
    println!(
        "{}",
        st.dim("The first column is what deleting the folder would give back: cloned data counts once,")
    );
    println!("{}", st.dim("and only when every copy is inside."));
    Ok(())
}

fn move_to(path: &Path, trash: &Path, name: &str) -> Result<(), String> {
    std::fs::create_dir_all(trash).map_err(|e| format!("couldn't make the Trash folder: {e}"))?;
    let mut dest = trash.join(name);
    let mut n = 2;
    while dest.exists() {
        dest = trash.join(format!("{name}-{n}"));
        n += 1;
    }
    std::fs::rename(path, &dest).map_err(|e| format!("couldn't move it to the Trash: {e}"))
}

fn main() {
    // Exit quietly when piped into head or less instead of panicking on the closed pipe.
    #[cfg(unix)]
    unsafe {
        extern "C" {
            fn signal(signum: i32, handler: usize) -> usize;
        }
        signal(13, 0); // SIGPIPE, SIG_DFL
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.first().map(String::as_str) {
        Some(c @ ("clean" | "size" | "help" | "--help" | "-h" | "--version" | "-V")) => (c, &args[1..]),
        _ => ("report", &args[..]),
    };
    let result = match cmd {
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Ok(())
        }
        "--version" | "-V" => {
            println!("reap {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "size" => cmd_size(rest),
        _ => Config::load(&home()).and_then(|cfg| {
            let a = parse(rest, &cfg)?;
            if cmd == "clean" {
                cmd_clean(&a, &cfg)
            } else {
                cmd_report(&a, &cfg)
            }
        }),
    };
    if let Err(e) = result {
        eprintln!("reap: {e}");
        std::process::exit(1);
    }
}
