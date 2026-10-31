//! Settings from ~/.config/reap/config (or the file named by REAP_CONFIG):
//!
//! ```text
//! root = ~/code                 # where to look when no folder is given; repeatable
//! xcode = true                  # also check Xcode's DerivedData
//! close ~/work = ~/work/bin/close-worktree {repo} {branch} {base}
//! ```
//!
//! A close hook hands finished worktrees in repositories under a folder to that team's own
//! tooling instead of the Trash. The placeholders {repo}, {path}, {branch} and {base} are
//! replaced with shell-quoted values, and the command runs with `sh -c`.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Hook {
    pub prefix: PathBuf,
    pub command: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    pub xcode: bool,
    pub hooks: Vec<Hook>,
}

fn expand(p: &str, home: &Path) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if p == "~" => home.to_path_buf(),
        None => PathBuf::from(p),
    }
}

impl Config {
    pub fn parse(text: &str, home: &Path) -> Result<Config, String> {
        let mut c = Config::default();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = || {
                format!(
                    "config line {}: expected `root = PATH`, `xcode = true` or `close FOLDER = COMMAND`",
                    n + 1
                )
            };
            let (key, value) = line.split_once(" = ").ok_or_else(bad)?;
            let (key, value) = (key.trim(), value.trim());
            match key.split_once(char::is_whitespace) {
                Some(("close", prefix)) => c.hooks.push(Hook {
                    prefix: expand(prefix.trim(), home),
                    command: value.to_string(),
                }),
                None if key == "root" => c.roots.push(expand(value, home)),
                None if key == "xcode" => c.xcode = matches!(value, "true" | "yes" | "on"),
                _ => return Err(bad()),
            }
        }
        Ok(c)
    }

    /// Settings from REAP_CONFIG or ~/.config/reap/config; none at all is fine.
    pub fn load(home: &Path) -> Result<Config, String> {
        let path = std::env::var_os("REAP_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config/reap/config"));
        match fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text, home).map_err(|e| format!("{}: {e}", path.display())),
            Err(_) => Ok(Config::default()),
        }
    }

    /// The close hook for a repository: the one with the longest matching folder.
    pub fn hook_for(&self, repo: &Path) -> Option<&Hook> {
        self.hooks
            .iter()
            .filter(|h| repo.starts_with(&h.prefix))
            .max_by_key(|h| h.prefix.components().count())
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The hook's command with its placeholders filled in.
pub fn render(command: &str, repo: &Path, path: &Path, branch: &str, base: &str) -> String {
    command
        .replace("{repo}", &shell_quote(&repo.display().to_string()))
        .replace("{path}", &shell_quote(&path.display().to_string()))
        .replace("{branch}", &shell_quote(branch))
        .replace("{base}", &shell_quote(base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_roots_xcode_and_hooks() {
        let home = Path::new("/home/me");
        let c = Config::parse(
            "# mine\nroot = ~/code\nroot = /srv/repos\nxcode = true\n\nclose ~/work = ~/work/bin/close {repo} {branch} {base}\n",
            home,
        )
        .unwrap();
        assert_eq!(c.roots, [PathBuf::from("/home/me/code"), PathBuf::from("/srv/repos")]);
        assert!(c.xcode);
        assert_eq!(c.hooks[0].prefix, PathBuf::from("/home/me/work"));
        assert_eq!(c.hooks[0].command, "~/work/bin/close {repo} {branch} {base}");
        assert!(Config::parse("colour = blue", home).unwrap_err().contains("line 1"));
    }

    #[test]
    fn the_most_specific_hook_wins_and_values_are_quoted() {
        let home = Path::new("/h");
        let c = Config::parse("close /w = outer\nclose /w/team = inner {branch}", home).unwrap();
        assert_eq!(c.hook_for(Path::new("/w/team/api")).unwrap().command, "inner {branch}");
        assert_eq!(c.hook_for(Path::new("/w/other")).unwrap().command, "outer");
        assert!(c.hook_for(Path::new("/elsewhere")).is_none());
        let cmd = render(
            "close {repo} {branch} {base}",
            Path::new("/w/it's"),
            Path::new("/x"),
            "fix/a b",
            "origin/main",
        );
        assert_eq!(cmd, r"close '/w/it'\''s' 'fix/a b' 'origin/main'");
    }
}
