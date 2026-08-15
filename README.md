# codex-rewind

**English** · [简体中文](https://github.com/extracurricular-ai/codex-rewind/blob/main/README.zh-CN.md)

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

```
/rewind     pick a prompt; the conversation and the files both return to it
/redo       undo that
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

Releases are versioned `<upstream>-rewind.<n>` — `0.147.0-rewind.1` is built from
upstream `rust-v0.147.0`, so the baseline each release carries is visible in its
version number. Being semver prereleases, they are also skipped by version *ranges*:
a `^0.147.0` dependency will never resolve to one by accident.

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

## Disk use

Snapshots live in `~/.codex/file_snapshots/`, content-addressed, so identical file
contents are stored once no matter how many turns or sessions share them. `/status`
shows the size. Deleting a conversation deletes its snapshots with it, contents
included.

## Reporting a bug

**`codexr --version` reports the upstream baseline, not the release.** It says
`0.147.0` where the release is `0.147.0-rewind.1`: the version compiled into the
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
