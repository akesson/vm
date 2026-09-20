# AGENTS.md

Notes for agents working in this repo. The README is the tool's documentation;
this file is about working on it.

## Build, test, lint

```sh
mise run build          # cargo build
mise run test           # cargo nextest run --features test-fake
mise run lint           # cargo fmt + clippy (--check for CI mode)
mise run test-real      # opt-in: the lane that drives real Parallels guests
```

`--features test-fake` is not optional for the test suite: the fake `prlctl`
the lifecycle tests drive is gated behind it so a plain `cargo install` never
ships it.

## Skills live here, and are used from two other places

This repo is the source of truth for the `vm` and `vm-upgrade` skills:

```
.agents/skills/vm/            .agents/skills/vm-upgrade/     ← edit these
.claude/skills                → symlink to ../.agents/skills
```

They are also installed at user level, once per agent, and the two are wired
**differently** — check which one you are touching before you copy anything:

| | what it is | after editing a skill |
|---|---|---|
| `~/.claude/skills/vm`, `~/.claude/skills/vm-upgrade` | symlinks into this repo | nothing to do — the edit is already live |
| `~/.codex/skills/vm`, `~/.codex/skills/vm-upgrade` | **copies** | re-copy, or Codex keeps reading the old text |

So: **after changing anything under `.agents/skills/`, refresh the Codex copy.**

```sh
rm -rf ~/.codex/skills/vm ~/.codex/skills/vm-upgrade
cp -R .agents/skills/vm .agents/skills/vm-upgrade ~/.codex/skills/
```

Verify with `diff -r .agents/skills/vm ~/.codex/skills/vm` (and the same for
`vm-upgrade`); silence means they are in sync.

Do **not** turn the `~/.claude/skills` entries into copies. `cp -R` alone is
harmless — it refuses (`Not a directory`, or `are identical (not copied)` for
the in-repo link) and exits 1, changing nothing. The way the link actually gets
lost is deleting it first: `rm -rf ~/.claude/skills/vm` followed by a copy
succeeds silently, and from then on Claude Code reads a snapshot that no longer
tracks this repo. If one of those links *is* missing, recreate it as a link:

```sh
ln -s "$PWD/.claude/skills/vm" ~/.claude/skills/vm
```

Two things that follow from the split:

- `~/.codex/skills/` is gitignored in the dotfiles repo that backs `~/.codex`,
  so the copy never shows up in `git status` over there.
- A skill must not hard-code `~/.claude/skills/...` as its own location — under
  Codex it is loaded from `~/.codex/skills/...`. `vm-upgrade` names both.

## Conventions worth knowing before you edit

- **Comments explain *why*, at the density of the surrounding code.** Much of
  this codebase is built around measured Parallels behaviour (argv size limits,
  stdin/EOF semantics, the cold-boot address ladder). Where a line exists
  because of a measurement, the comment says what was measured.
- **Exit codes are a contract**: 125 = vm's own infra failure, 2 = usage/config,
  anything else is the guest command's own. See `src/exit.rs`.
- **Tests name the failure they prevent**, not the function they call.
