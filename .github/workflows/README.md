# Workflows

Three things run here: the verification this repository owns, the verification
it inherited, and building binaries.

Actions is free and unmetered for public repositories on standard runners, so
there is no minute budget to protect — an earlier version of this file assumed
otherwise and cut CI down to nothing, which traded away the only check on an
upstream merge for a saving that did not exist.

## The checks this repository owns — `tests.yml`

**`tests / all` is the status to require.** Everything else here is either
upstream's or advisory.

Runs on every pull request, on pushes to `main`, and — through a `uses:` in
`release.yml` — before anything is published to npm. Five jobs and a gatherer:

| job | where | what |
| --- | --- | --- |
| `snapshots` | ubuntu, macOS, Windows | `cargo test -p codex-file-snapshots --all-targets` — 34 unit tests and 18 integration tests |
| `lint` | ubuntu | `cargo fmt` and `cargo clippy -D warnings`, scoped to that crate |
| `wiring` | ubuntu | the three end-to-end tests in `codex-core` that prove the capture hook is attached to a real turn, then a compile check of `codex-tui` and `codex-app-server`, where `/rewind` and `/redo` live |
| `launcher` | ubuntu, macOS, Windows × Node 20/22/24 | `codex-cli/test/launcher.test.mjs` — runs the real `bin/codex.js` against a staged package with a stub binary in it |
| `package` | ubuntu | stages and packs the launcher exactly as a release would, checks the tarball would install, and starts it |

The scope is deliberate: the crate this distribution adds, everything in the
workspace that consumes it, and the launcher and package it ships. Upstream's
other crates are upstream's to verify.

What is **not** covered, so nobody reads more into a green tick than is there:
the Bazel build of `file-snapshots` (upstream's BuildBuddy infrastructure), and
any test that needs a real `codexr` binary — a release build is 1,386 crates and
does not belong on a pull request, so the launcher and package jobs use stubs.

Three platforms because the snapshot store writes real files — path separators,
the executable bit, case-insensitive filesystems, and Windows holding a handle
open past the last close are things only the target OS can answer. The launcher
matrix is three platforms for the same reason: it picks a target triple from
`process.platform`, so it can only be tested on the platform it is deciding for.

No `paths:` filter, on purpose. A required check that gets skipped never
reports, and a pull request waiting on a status that will never arrive cannot
be merged at all.

### Why it is a separate file

`.github/` is the hottest conflict path in every upstream sync, so the checks
this fork depends on live where upstream will never touch them. Keeping them
out of `blocking-ci.yml` is the same argument twice: that workflow's `required`
gate is permanently red here, and a signal muxed into a check that is always red
is not a signal.

## Inherited verification — `blocking-ci.yml`

Runs on every pull request and on pushes to `main`, and calls `rust-ci`,
`repo-checks`, `codespell` and `cargo-deny`.

Its `required` job is **not** the one to require here. `rust-ci` asks for
`macos-15-xlarge` and for a self-hosted group named `<repo>-runners`, neither of
which this account has, so that gate is red whatever the change did. What is
worth reading inside it is `repo-checks`, `codespell` and `cargo-deny`, which
all run on standard runners.

`rust-ci` also runs no tests at all: it is `cargo fmt`, a benchmark smoke test,
`cargo shear` and the argument-comment lint. Every `cargo test` and every
`cargo clippy` upstream has lives in `rust-ci-full.yml`, on runners this
repository does not have — which is the gap `tests.yml` exists to close.

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

Manual only, and it has **no path to npm at all**. That is the point of the
split: a build you started to answer "does this compile" can never turn into a
release.

*Actions → build → Run workflow*. `targets` accepts any comma-separated subset
of `linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`, `win32-x64`,
`win32-arm64`, or `all`. Leave `release_tag` blank for plain artifacts.

> The Run workflow button only appears once the file is on the repository's
> **default branch**. On any other branch the workflow exists but has nowhere
> to be triggered from — this catches everyone once.

Picking a subset is about wall-clock time, not money: a cold release build of
this workspace is 1,386 crates with full optimisation, so one platform is a
much faster way to check that something compiles than six.

### Getting the binaries

Each target uploads `codexr-<triple>.tar.gz`, kept for 14 days. Download them
from the run page, or:

```bash
gh run download <run-id> -D vendor/
```

Each archive holds `<triple>/bin/codexr`. That layout is not arbitrary — it is
where the launcher looks, and what `--vendor-src` copies through verbatim:

```
vendor/
  aarch64-unknown-linux-musl/bin/codexr
  x86_64-apple-darwin/bin/codexr
```

## Releasing — `release.yml`

Push a tag matching `v*-rewind.*`. It calls `tests.yml` and `build.yml`, the
latter for all six platforms, attaches the binaries to a draft release, and
**publishes to npm** — but only if every job in `tests.yml` passed. A `uses:`
job succeeds only when the whole called workflow did, so that gate is the
entire matrix, not a summary of it.

The two run alongside each other rather than in sequence: `build.yml` has no
path to npm, so the only step worth gating is the one that has.

```bash
git tag v0.147.0-rewind.0
git push dist v0.147.0-rewind.0
```

Tagging is the deliberate act that means *publish this*. An npm version cannot
be replaced once it exists, so push the tag when you mean it.

To rehearse, or to retry after a tag build failed partway: *Actions → release →
Run workflow*, give it the existing tag, and leave `dry_run` on. That packs and
validates all seven tarballs against the registry without uploading.

### What actually gets published

Seven versions of **one** package, `codex-rewind`:

| version | dist-tag |
| --- | --- |
| `0.147.0-rewind.0-linux-x64`, and five more like it | `linux-x64`, … |
| `0.147.0-rewind.0` — the launcher | `latest` |

The platform builds are deliberately kept off `latest`. That tag is what a bare
`npm install` follows, so pointing it at a platform build would hand every
other platform a package with no binary it can run. The launcher is published
last, and only once all six resolve — its `optionalDependencies` name them by
exact version.

### Auth

Set `NPM_TOKEN` under *Settings → Secrets and variables → Actions* — a granular
access token with publish rights is enough.

Once the package exists you can drop the token and switch to trusted publishing
(OIDC) on its npm settings page, which also attaches provenance showing which
commit and workflow produced each version. It cannot be set up beforehand: the
package has to exist to have a settings page, which is why a first publish is
always token-based.

## Why the Linux targets are musl

So one binary runs on any distribution without needing a matching glibc. The
runner installs `musl-tools` for it.
