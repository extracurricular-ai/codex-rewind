# Workflows

Two things run here: verification, and building binaries.

Actions is free and unmetered for public repositories on standard runners, so
there is no minute budget to protect — an earlier version of this file assumed
otherwise and cut CI down to nothing, which traded away the only check on an
upstream merge for a saving that did not exist.

## Verification — `blocking-ci.yml`

Runs on every pull request and on pushes to `main`, and calls `rust-ci`,
`repo-checks`, `codespell` and `cargo-deny`. The `required` job inside it is the
one to mark as a required status check; it is written to fail loudly rather than
appear green when a dependency was skipped.

`rust-ci-full.yml` is the heavier cross-platform suite. It is reachable three
ways: called by `rust-ci`, run by hand, or triggered by pushing any branch whose
name contains **`full-ci`**. That last one is the useful one after an upstream
merge, where the dangerous failure is a merge that resolved cleanly and is
semantically wrong — nothing but the integration tests will catch it.

What stays deleted, and why: upstream's five `rust-release*` workflows need
self-hosted runners, Apple notarisation, Azure Key Vault signing and R2
credentials; `bazel.yml` cannot be verified from this side; the issue bots, CLA
check, SDK, V8 and Python pipelines are upstream's own infrastructure. So is
`dependabot.yaml` — six ecosystems on a weekly schedule is noise for a
distribution that takes its dependency updates through the upstream sync.

## Building binaries — `build.yml`

Never runs on a branch push or a pull request. It starts two ways.

## Cutting a release: push a tag

```bash
git tag v0.147.0-rewind.0
git push dist v0.147.0-rewind.0
```

Any tag matching `v*-rewind.*` builds **all six platforms** and attaches them to
a draft release for that tag. The pattern is narrow on purpose: a stray tag
should not start a six-platform matrix and leave a half-built release behind.

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

Picking a subset is about wall-clock time, not money: a cold release build of
this workspace is 1,386 crates with full optimisation, so one platform is a
much faster way to check that a change compiles than six.

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

## Publishing to npm

Publishing is never automatic — not even on a tag. An npm version cannot be
replaced once it exists, so it stays a separate, deliberate run made *after*
looking at what the tag built.

*Actions → build → Run workflow*, with:

    targets      = all          (required — see below)
    release_tag  = v0.147.0-rewind.0
    publish      = checked
    dry_run      = checked      (leave on for the first attempt)

A dry run packs all seven tarballs and validates them against the registry
without uploading. Re-run with `dry_run` off to publish for real.

`targets` must be `all`. The launcher names each platform build by exact
version in its `optionalDependencies`, so publishing a partial set ships a
package that cannot install anywhere it did not build — and no version can be
replaced to fix it.

### What actually gets published

Seven versions of **one** package, `codex-rewind`:

| version | dist-tag |
| --- | --- |
| `0.147.0-rewind.0-linux-x64`, and five more like it | `linux-x64`, … |
| `0.147.0-rewind.0` — the launcher | `latest` |

The platform builds are deliberately kept off `latest`. That tag is what a bare
`npm install` follows, so pointing it at a platform build would hand every
other platform a package with no binary it can run. The launcher is published
last, and only once all six resolve.

### Auth

Set `NPM_TOKEN` under *Settings → Secrets and variables → Actions* — a granular
access token with publish rights is enough.

Once the package exists you can drop the token and switch to trusted publishing
(OIDC) on its npm settings page, which also attaches provenance showing which
commit and workflow produced each version. It cannot be set up beforehand: the
package has to exist to have a settings page, which is why a first publish is
always token-based.

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
