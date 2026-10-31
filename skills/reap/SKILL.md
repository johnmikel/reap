---
name: reap
description: Free disk space taken by finished git worktrees and idle build caches, safely. Use when the user asks why their disk is full, wants to clean up worktrees an agent left behind, or asks how much space a folder really uses on a Mac.
---

# reap

`reap` finds git worktrees and build folders that hold nothing anyone would lose, and reports what deleting them would really free. On macOS it counts cloned (copy-on-write) data once, so its numbers are often far lower than `du`'s.

## Steps

1. Check it's installed: `reap --version`. If not, tell the user it installs with `brew install johnmikel/tap/reap` or `cargo install --git https://github.com/johnmikel/reap`, and stop.
2. Run the report: `reap` (or `reap PATH` for a specific folder). It only reads; it changes nothing.
3. Tell the user in plain words what is safe to clear, how much space that really frees, and why the other worktrees were kept.
4. Ask before clearing. Only after the user says yes, run `reap clean --yes` with the same folder and options as the report.
5. Say where things went: worktrees with a close hook were closed by it; everything else is in the Trash under "reap <date>" until they empty it.

To answer "how big is this folder really?", run `reap size PATH...`. It reads only.

## Rules

- Never run `reap clean` without the user's explicit yes in this conversation.
- Never empty the Trash or delete anything reap kept. Each kept worktree has a reason: uncommitted changes, unmerged commits, an env file of its own, a program running in it, or recent activity.
- Never clean the worktree you are working in. reap skips it on its own, because your process runs inside it.
