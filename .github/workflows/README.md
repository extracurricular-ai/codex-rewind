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

Push a tag matching `v*-rewind.*`. It calls `build.yml` for all six platforms,
attaches them to a draft release, and **publishes to npm**.

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
