# Workflows

One workflow: `build.yml`. Everything else upstream ships was removed.

Actions minutes are limited on this repository, so it never runs on a push to a
branch and never on a pull request. It starts two ways.

## Cutting a release: push a tag

```bash
git tag v0.147.0-rewind.0
git push dist v0.147.0-rewind.0
```

Any tag matching `v*-rewind.*` builds **all six platforms** and attaches them to
a draft release for that tag. Tags are pushed rarely and deliberately, so this
spends quota exactly when a release is wanted and never otherwise. The pattern
is narrow on purpose: a stray tag cannot start a six-platform matrix.

## Building some platforms: run it by hand

**In the browser** — *Actions → build → Run workflow*, on the right. Fill in
`targets` and leave `release_tag` blank to get plain artifacts.

> The button only appears once `build.yml` is on the repository's **default
> branch**. On any other branch the workflow exists but has nowhere to be
> triggered from — this catches everyone once.

Or from the CLI, if you have `gh`:

```bash
gh workflow run build.yml -f targets=linux-x64,linux-arm64
gh workflow run build.yml -f targets=all -f release_tag=v0.147.0-rewind.0
```

`targets` accepts any comma-separated subset of `linux-x64`, `linux-arm64`,
`darwin-x64`, `darwin-arm64`, `win32-x64`, `win32-arm64`, or `all`.

**Runner minutes are not billed equally.** GitHub charges Linux at 1x, Windows
at 2x and macOS at **10x**, so `targets=all` costs about 25 Linux-minutes for
every wall-clock minute. Build the platform you are actually going to run, and
leave `all` for a release.

## Getting the binaries

Each target uploads `codexr-<triple>.tar.gz`, kept for 14 days:

```bash
gh run download                        # latest run, into ./
gh run download <run-id> -D vendor/    # a specific run
```

Each archive contains one directory named after the target triple, holding
`codexr`. Unpacked side by side they form the layout the npm packaging script
expects:

```
vendor/
  aarch64-unknown-linux-musl/codexr
  x86_64-apple-darwin/codexr
  ...
python3 codex-cli/scripts/build_npm_package.py --package codex-linux-arm64 \
  --version 0.147.0-rewind.0 --vendor-src vendor --pack-output out.tgz
```

## Attaching them to a release

Both routes attach to a release, and both create it **as a draft** — a matrix
that fails halfway then never looks like a finished release. Review it and
publish yourself.

On a manual run the draft has a second effect: a draft release does not create
the git tag, so the tag appears only when you publish. That is deliberate —
`<upstream>-rewind.<n>` should only ever name something that was really built.
(Pushing a tag obviously creates it first; there the draft is just the review
step.)

## Why the Linux targets are musl

So one binary runs on any distribution without needing a matching glibc. The
runner installs `musl-tools` for it.
