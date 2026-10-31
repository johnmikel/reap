# reap

Clears git worktrees and build caches that hold nothing you'd lose, and tells you how much space that really frees.

On one Mac with 251 worktrees, `du` said 74 GB could be cleared. reap measured 23 GB. macOS shares unchanged files between copies (APFS clones), and `du` counts every copy in full. A cautious clean (worktrees idle for a week, build folders in projects idle for a month) took free space from 100 GiB to 110 GiB.

```bash
cargo install --git https://github.com/johnmikel/reap
reap          # report only: what is safe to clear, and the space it really frees
reap clean    # list it again, ask, then clear
```

## What counts as safe

reap offers a worktree only when it holds nothing that exists nowhere else:

- It has no uncommitted or untracked changes.
- Every commit is merged into the remote's main branch. In repositories without a close hook, pushed to any remote branch also counts.
- It has no env file (`.env`, `.env.local`, `.dev.vars`, …) that differs from the main checkout's copy.
- It has had no git activity for `--idle` days (default 3), since another person or agent session may be working in it.

reap offers build folders (`node_modules`, Rust `target`, `.next`, `.turbo`, `.expo`, `.venv`, `Pods`, …) once their project has been idle for `--days` (default 30). With `--xcode`, it also offers Xcode DerivedData that hasn't been touched for two weeks.

reap reads git with `--no-optional-locks`, so a scan never rewrites the index of a worktree someone else is using.

## How clean works

`reap clean` lists everything again and asks before touching anything. Folders move to a dated `reap <date>` folder in the Trash, and reap prunes git's record of each moved worktree. Nothing is deleted outright: the space comes back when you empty the Trash.

## Close hooks

Some teams close worktrees with their own script, one that also deletes the merged branch or updates a tracker. You can point reap at it in `~/.config/reap/config`:

```text
root = ~/code                 # where to look when no folder is given
xcode = true                  # also check Xcode's DerivedData
close ~/work = ~/work/scripts/close-worktree {repo} {branch} {base}
```

For repositories under `~/work`, finished worktrees go through that command instead of the Trash. reap fills in `{repo}`, `{path}`, `{branch}` and `{base}`, shell-quoted. It hands over only worktrees merged into the base, and the hook's own checks have the last word: anything the hook refuses is listed and left alone.

## Sizes

For each file, reap asks macOS how many bytes only that file holds, and which clone family it belongs to. Shared data counts once, and only when every file sharing it is inside what's being cleared. Hard-linked files count toward the `du` figure only, since their other links may live elsewhere. The report shows both numbers.

Clone-aware sizing needs macOS and APFS. On other systems, reap does everything else the same but reports allocated sizes, like `du`.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `ROOT...` | config roots, else this folder | where to look for repositories and projects |
| `--idle N` | 3 | days without git activity before a finished worktree counts |
| `--days N` | 30 | days a project must be idle before its build folders count |
| `--xcode` | off | also check Xcode's DerivedData |
| `--all` | off | list every item, kept worktrees and their reasons included |
| `--dry-run` | off | with `clean`: show the list and stop |
| `--yes` | off | with `clean`: skip the question |

MIT licensed. It uses only the Rust standard library.
