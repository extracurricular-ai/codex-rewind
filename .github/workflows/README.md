# Workflows

One workflow: `build.yml`. Everything else upstream ships was removed.

Actions minutes are limited on this repository, so nothing runs automatically —
no push triggers, no pull-request triggers. You start a build when you want
binaries, and you name the platforms you want.

## Building

*Actions → build → Run workflow*, or:

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

Pass `release_tag`. The workflow creates that release **as a draft** if it does
not exist and uploads each archive as it finishes, so a matrix that fails
halfway never looks like a finished release. Review the draft and publish it
yourself.

Note that a draft release does not create the git tag — the tag appears only
when you publish. That is deliberate: `<upstream>-rewind.<n>` tags are supposed
to name something that was actually built.

## Why the Linux targets are musl

So one binary runs on any distribution without needing a matching glibc. The
runner installs `musl-tools` for it.
