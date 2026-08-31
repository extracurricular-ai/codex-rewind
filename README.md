# codex-rewind

**English** · [简体中文](https://github.com/extracurricular-ai/codex-rewind/blob/main/README.zh-CN.md)

[![npm](https://img.shields.io/npm/v/codex-rewind?label=npm&color=cb3837)](https://www.npmjs.com/package/codex-rewind)
[![downloads](https://img.shields.io/npm/dm/codex-rewind?label=downloads&color=2f855a)](https://www.npmjs.com/package/codex-rewind)
[![licence](https://img.shields.io/badge/licence-Apache--2.0-blue)](LICENSE)
[![platforms](https://img.shields.io/badge/platforms-macOS%20%C2%B7%20Linux%20%C2%B7%20Windows-informational)](#install)

**An unofficial distribution of [OpenAI Codex CLI](https://github.com/openai/codex).**

> For what Codex CLI is, how to sign in, and how to use it, read
> **[the official README](https://github.com/openai/codex#readme)**. Everything there
> applies here. This page only covers what this distribution adds.
>
> Not affiliated with, endorsed by, or supported by OpenAI. Apache-2.0, same as
> upstream. Report problems here, not to OpenAI — and include the npm version, which
> is the only one that names the release. See [Reporting a bug](#reporting-a-bug).

---

## What this adds: rewind

Upstream Codex can take you back to an earlier point in a **conversation**. It cannot
take your **files** back with it — so the model ends up reasoning from a conversation
that predates the code sitting on disk.

This distribution adds the missing half. `/rewind` restores the workspace to how it
looked at the prompt you pick, and `/redo` puts it back if you change your mind.

![The agent deletes notes.txt; /rewind picks the prompt before that; ls shows the file back.](https://raw.githubusercontent.com/extracurricular-ai/codex-rewind/main/.github/rewind.gif)

▶ [Full walkthrough (22 min)](https://youtu.be/OpJI8NQ-mvY) — the demo above in full,
then the design behind it: why git is the wrong foundation, what the three buckets are,
and where it stops.

```
/rewind     pick a prompt; the conversation and the files both return to it
/redo       undo that
/status     shows whether snapshots are on, and what they cost on disk
```

No git required, and it never touches your git state — no commits, no stashes, no
index writes, nothing inside `.git`. It works in directories that are not
repositories at all.

## Install

```shell
npm install -g codex-rewind
```

The command is **`codexr`**, not `codex`, so this installs alongside the official
build rather than replacing it.

```shell
codexr          # this distribution
codex           # the official one, if you have it
```

Ships prebuilt binaries for macOS, Linux and Windows on both x64 and arm64, the same
targets as upstream. Node 16 or newer.

Removing it again is two commands, and leaves your conversations untouched — see
[Clean uninstall](#clean-uninstall).

Releases are versioned `<upstream>-rewind.<n>` — `0.151.0-rewind.1` is built from
upstream `rust-v0.151.0`, so the baseline each release carries is visible in its
version number. Being semver prereleases, they are also skipped by version *ranges*:
a `^0.151.0` dependency will never resolve to one by accident.

The baseline half is what `codexr --version` reports. The `-rewind.<n>` half lives
only in the npm package. See [Reporting a bug](#reporting-a-bug).

## Enable it

Rewind is off by default. To try it on a single run, without changing any file:

```shell
codexr --enable file_snapshots
```

To keep it on, turn it on in `/experimental`, or:

```toml
# ~/.codex/config.toml
[features]
file_snapshots = true
```

It binds **per session**: enabling affects new sessions only, and disabling never
stops a session that is already tracking. So a session either has snapshots for its
whole life or has none — there is no half-tracked state to reason about.

## ⚠️ Sharing `~/.codex` with the official build

This distribution deliberately uses the **same** `~/.codex` directory as official
Codex, so your login, config, and conversation history carry over and you do not
have to sign in twice.

The cost is worth understanding:

- **Opening a rewind-tracked session with the official `codex` breaks tracking for
  it.** The official build knows nothing about snapshots. It will happily continue
  the conversation, and every turn it runs is a turn with no checkpoint behind it —
  so a later `/rewind` in `codexr` can restore the workspace only as far as the last
  turn *this* build saw. There is no error and no warning; the gap is silent.
- Conversations started in the official build have no snapshots at all. `/rewind`
  there falls back to conversation-only, exactly as upstream behaves.
- The official build logs `unknown feature key in config: file_snapshots` and
  ignores the `[file_snapshots]` section. Harmless, but you will see it.

**If you use both, finish a conversation in the build you started it in.** If you
would rather keep them fully apart, point this one somewhere else:

```shell
CODEX_HOME=~/.codex-rewind codexr
```

You will sign in again in that directory, and the two builds will then share
nothing.

## Clean uninstall

Two commands remove the program and all of the data it created:

```shell
npm uninstall -g codex-rewind     # the program
rm -rf ~/.codex/file_snapshots    # the snapshots
```

### Everything it puts on your disk

The complete list of what exists because of this build and not the official one, so
you can check rather than take the two commands on faith:

| Path | What it is | Removed by |
| --- | --- | --- |
| `~/.codex/file_snapshots/` | the snapshot store: `blobs/`, `manifests/`, `refs/`, `turns/`, `restores/` | `rm -rf` above |
| `<npm prefix>/lib/node_modules/codex-rewind/` | the launcher, and the native binary nested inside it under `node_modules/` — about 300 MB together | `npm uninstall` above |
| `<npm prefix>/bin/codexr` | the symlink that puts `codexr` on your PATH | `npm uninstall` above |
| `[features] file_snapshots` in `config.toml` | the on switch | by hand, below |
| `[file_snapshots]` in `config.toml` | tuning, only if you set any | by hand, below |
| `~/.npm/_cacache` entries | npm's own download cache, which `npm uninstall` never clears | `npm cache clean --force`, which clears it for *every* package |

Two details worth stating rather than letting you find them:

- `~/.codex/file_snapshots/` is created the first time you run `codexr`, **whether or
  not you ever enabled the feature** — the store is opened before the check that
  decides whether this session tracks anything. If you never turned it on, the
  directory is there and empty.
- `/status` shows the store's size, but only when the feature is on.

Two more live in your **project**, not in `~/.codex`, and neither is created without
you:

- `.codexsnapignore` — only if you wrote one. It is yours; delete it like any other
  file in your repository.
- `<file>.<pid>.<n>.codex-restore-tmp` — a restore's temp file, removed whether the
  restore succeeds or fails. One can only survive a crash mid-restore, and it is
  safe to delete.

Nothing else. No shell rc line, no PATH edit beyond npm's own, no cache, no state
directory of its own, and nothing written next to the binary. `~/.codex/sessions/`,
`~/.codex/archived_sessions/`, `~/.codex/log/` and the rest of `~/.codex` are
upstream's and are shared with the official build — leave them alone.

npm is the only route this ships through. The standalone installer scripts in this
repository are upstream's — they install the official `codex` binary from
releases.openai.com and have nothing to do with this build.

Optionally, remove two entries from `~/.codex/config.toml`:

```toml
[features]
file_snapshots = true    # this line

[file_snapshots]         # and this section, if you set anything in it
track_hidden_files = true
```

Leaving them normally costs one log line: the official build warns `unknown feature
key in config: file_snapshots` and ignores the `[file_snapshots]` table silently.
The exception is `codex --strict-config`, which rejects unknown configuration fields
outright — under that flag the leftover key is a hard error, not a warning.

**Do not delete `~/.codex` itself.** The official build uses the same directory —
your login, your config and every conversation you have ever had live there.

### Your conversations are not part of this

History is written in upstream's own format, and this distribution **adds nothing to
it**: no extra event type, no extra field, no schema change. Snapshots live entirely
under `file_snapshots/`, keyed by session and turn id — they point *at* your history
rather than being part of it.

So a conversation you ran here, rewound and all, opens in the official `codex`
exactly like any other, before or after you uninstall. Deleting `file_snapshots/`
takes away the ability to rewind those turns, and nothing else.

One thing worth knowing because it is visible: `/rewind` **archives** the
conversation it supersedes rather than deleting it, into
`~/.codex/archived_sessions/`. The directory and the archive mechanism are upstream's,
so the official build reads them and `codex unarchive <id>` brings one back — but
archiving *on rewind* is this build's own decision, and uninstalling does not undo it.
So after removing this, conversations you rewound are still there and still yours,
and they are in the archive rather than in `/resume`'s list until you unarchive them.

## "Why not just commit before every turn?"

People do, and they say it is a hassle
([#19205](https://github.com/openai/codex/issues/19205)). Two reasons it is not a
substitute:

- **Someone has to remember, every single turn.** A person doing it calls it a hassle,
  which is what #19205 is. A model doing it will sometimes forget. A checkpoint should
  happen whether anybody thought of it or not.
- **Git only sees files git already knows about.** The incidents behind
  [#9203](https://github.com/openai/codex/issues/9203) are untracked files —
  spreadsheets, notes, generated data. `git status` is clean and there is nothing to
  restore from.

The design reasoning, the measurements behind the three-partition bound, and the
correctness rules are in the
**[RFC](https://github.com/extracurricular-ai/codex-rewind/blob/main/docs/rfc-file-snapshot-rewind.md)**.

## What gets tracked

Three sources, unioned, each bounded by something other than the size of your
directory tree — so the cost does not grow with how long you have been building in
a repository:

| | |
| --- | --- |
| **Files git tracks** | read from the index. No cap: what your project committed is your project's, however much of it there is. |
| **Files the agent edits** | captured from the edit tool, wherever they live — including outside the working directory. No cap. |
| **Recently modified files** | the residue, for shell-made changes to everything else. Capped at 100 files, 16 MB each, skipping `node_modules`, `target`, `Pods` and the like. |

Hidden files are left alone by default — `.env`, `.vscode/`, virtualenvs and caches
are tool state, not your work, and rolling them back with a turn would be a nasty
surprise. `.git` is never read. Files the agent explicitly edits are tracked even if
hidden, because those *are* your work.

To exclude more, add a `.codexsnapignore` (gitignore syntax). It is deliberately
separate from `.gitignore`: an ignored path is never snapshotted, never restored,
and **never deleted** by a restore.

## Coming from Claude Code

Claude Code has had checkpointing for a while, and if that is your reference point,
these are the differences that matter:

| | Claude Code `file-history` | codex-rewind |
| --- | --- | --- |
| What is in scope | files its own edit tools touched | that, **plus** the git index, **plus** the 100 most recently changed |
| Changes made by a shell command | not tracked | caught by the recency bucket |
| How far back you can go | the 100 most recent checkpoints in a session | no cap — turns are not discarded to make room |
| Storage | per-file copies | content-addressed and deduplicated |
| Requires git | no | no |

The unbounded part is the same on both sides: neither puts a limit on how many files
the agent may edit. What differs is what *else* is in scope, and whether anything gets
thrown away to make room.

<sub>Claude Code figures quoted from its own documentation (Claude Code Docs →
Checkpointing), current as of August 2026. Its own limitations page is where "bash
command changes not tracked" and "external changes not tracked" come from.</sub>

## What it will not do

- **Restore a file no snapshot ever saw.** Deleting needs positive evidence that the
  file was absent — a capture that looked and did not find it. Nothing is inferred
  from a path merely being missing, because guessing wrong destroys work that was
  never the agent's to remove.
- **Restore work from before it knew a file existed.** Files outside the working
  directory enter tracking when the agent first touches them, so a prompt from before
  that moment has no copy to give back. Rewinding there says so, and tells you to
  pick a more recent prompt.
- **Merge concurrent sessions.** Two sessions in one directory can overwrite each
  other's files. `/redo` warns and names the files before it does, but it does not
  merge. Use a worktree or a separate checkout.
- **Work on remote environments.** Local only.

## If you would rather check than trust

This runs against your files, so scepticism is the correct posture. Two documents
are kept so you can audit the reasoning rather than take a README's word for it:

- **[RFC](https://github.com/extracurricular-ai/codex-rewind/blob/main/docs/rfc-file-snapshot-rewind.md)**
  — what the system is: the correctness rules, the three-partition bound and the
  measurements behind it, and why deletion needs positive evidence.
- **[Decision log](https://github.com/extracurricular-ai/codex-rewind/blob/main/docs/file-snapshots-decision-log.zh.md)**
  — what it is *not*, and why. Every approach that was tried and overturned, with
  dates, the reason it was overturned, and explicit **"do not revert to this"**
  markers on the ones that still look reasonable. It records the bugs found in
  this code and the platform traps behind them, including the ones found by its
  own author. In Chinese.

The second one is the more useful of the two if what you want to know is whether
anybody thought hard about the ways this could lose your work.

## Disk use

Snapshots live in `~/.codex/file_snapshots/`, content-addressed, so identical file
contents are stored once no matter how many turns or sessions share them. `/status`
shows the size. Deleting a conversation deletes its snapshots with it, contents
included.

## Spin-offs

Two projects that grew out of this one, for when a Codex distribution is not the
shape you need:

- **[filesnap](https://github.com/extracurricular-ai/filesnap)** — the snapshot engine on
  its own, with no agent wrapped around it. A Rust crate and CLI
  (`cargo install filesnap-cli`): content-addressed store, `capture` and `restore` keyed
  by session and turn, JSON Lines output for scripting, `.filesnapignore`, and the same
  rule that a file no snapshot ever saw is never deleted. Reach for it to put rewind into
  something of your own.
- **[dsh-filesnap](https://github.com/extracurricular-ai/dsh-filesnap)** — the same
  `/rewind` and `/redo`, as a plugin for DeepSeek Harness:
  `dsh plugin --profile web add dsh-filesnap`. Per-turn workspace snapshots, rewind
  controls in the browser UI, and git left alone here too.

## Reporting a bug

**`codexr --version` reports the upstream baseline, not the release.** It says
`0.151.0` where the release is `0.151.0-rewind.1`: the version compiled into the
binary comes from the upstream workspace, and the `-rewind.<n>` suffix is added
during npm packaging. Two releases on the same baseline report the same number.

So take the release from npm:

```shell
npm ls -g codex-rewind
```

Include both in a bug report. The npm version identifies the release; `codexr
--version` confirms which upstream it was built from.

## Contributing

Sign your commits off: `git commit -s`.

> If a change here is ever proposed to openai/codex, its original author must sign
> OpenAI's CLA personally. Maintainers cannot do it for you.

## Licence

Apache-2.0, inherited from upstream, with the `NOTICE` file intact and changes
stated as §4 requires. No OpenAI trademark or endorsement is claimed.
