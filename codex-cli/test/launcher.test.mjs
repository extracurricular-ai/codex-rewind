// Tests for `bin/codex.js`, the launcher every `codexr` invocation goes
// through.
//
// It is the one piece of this distribution that runs on a user's machine and
// that nothing else covers: the Rust tests never see it, and until a release
// tag is pushed nothing executes it at all. It is also where a rename bites
// hardest — the six platform package names are written once here, once in
// `bin/codex.js`, and once in `scripts/build_npm_package.py`, and npm resolves
// them by string.
//
// The stub binary is a copy of the running Node executable. That is the one
// program guaranteed to exist and to be executable on all three platforms CI
// covers: a shell script would not spawn on Windows, where the launcher looks
// for `codexr.exe` specifically.
//
// Everything the stub is asked to do is passed as `-e` source, never as a
// script path, and that is load-bearing rather than stylistic. The stub is
// Node, so it will happily execute any file path handed to it — including the
// launcher's own — and the launcher forwards its arguments verbatim. Hand it a
// path and a re-entry loop is one mistake away; `-e` cannot re-enter anything.

import { strict as assert } from "node:assert";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const LAUNCHER = path.join(HERE, "..", "bin", "codex.js");

// Stated here independently of the launcher, on purpose. If `bin/codex.js`
// changes which triple it looks under, the stub lands somewhere it does not
// look and these tests fail — which is the alarm, not an inconvenience.
const TRIPLE_BY_HOST = {
  "linux-x64": "x86_64-unknown-linux-musl",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
  "win32-x64": "x86_64-pc-windows-msvc",
  "win32-arm64": "aarch64-pc-windows-msvc",
};

const PACKAGE_BY_TRIPLE = {
  "x86_64-unknown-linux-musl": "codex-rewind-linux-x64",
  "aarch64-unknown-linux-musl": "codex-rewind-linux-arm64",
  "x86_64-apple-darwin": "codex-rewind-darwin-x64",
  "aarch64-apple-darwin": "codex-rewind-darwin-arm64",
  "x86_64-pc-windows-msvc": "codex-rewind-win32-x64",
  "aarch64-pc-windows-msvc": "codex-rewind-win32-arm64",
};

const HOST = `${process.platform}-${process.arch}`;
const TRIPLE = TRIPLE_BY_HOST[HOST];

// Source run by the stub binary, reporting everything the launcher is supposed
// to have done to it: the arguments it forwarded, the environment it composed,
// and — through PROBE_EXIT — the status it should mirror back.
//
// `slice(1)`, not `slice(2)`: under `node -e <source> a b` there is no script
// path in the argument vector, so the forwarded arguments start at index 1.
const PROBE = `
const report = {
  args: process.argv.slice(1),
  root: process.env.CODEX_MANAGED_PACKAGE_ROOT ?? null,
  npm: process.env.CODEX_MANAGED_BY_NPM ?? null,
  pnpm: process.env.CODEX_MANAGED_BY_PNPM ?? null,
  bun: process.env.CODEX_MANAGED_BY_BUN ?? null,
  vitePlus: process.env.CODEX_MANAGED_BY_VITE_PLUS ?? null,
};
console.log(JSON.stringify(report));
process.exit(Number(process.env.PROBE_EXIT ?? "0"));
`;

/**
 * Build the package layout the launcher expects to be installed into: the
 * launcher at `bin/codex.js`, and (unless asked otherwise) a stub binary at
 * `vendor/<triple>/bin/codexr`, which is the fallback path taken when no
 * platform package resolves.
 */
function stagePackage({ withBinary = true } = {}) {
  // realpath first: macOS hands out `/var/folders/...`, a symlink to
  // `/private/var/...`, and the launcher reports the resolved path back.
  const root = fs.mkdtempSync(
    path.join(fs.realpathSync(os.tmpdir()), "codexr-launcher-"),
  );

  fs.mkdirSync(path.join(root, "bin"), { recursive: true });
  fs.copyFileSync(LAUNCHER, path.join(root, "bin", "codex.js"));

  if (withBinary) {
    const binDir = path.join(root, "vendor", TRIPLE, "bin");
    fs.mkdirSync(binDir, { recursive: true });
    const stub = path.join(
      binDir,
      process.platform === "win32" ? "codexr.exe" : "codexr",
    );
    fs.copyFileSync(process.execPath, stub);
    fs.chmodSync(stub, 0o755);
  }

  return root;
}

function cleanup(root) {
  // Windows can still hold a handle on the stub it just ran, so retry rather
  // than fail the test on teardown.
  fs.rmSync(root, {
    recursive: true,
    force: true,
    maxRetries: 5,
    retryDelay: 100,
  });
}

function runLauncher(root, args, env = {}) {
  return spawnSync(
    process.execPath,
    [path.join(root, "bin", "codex.js"), ...args],
    {
      encoding: "utf8",
      env: { ...process.env, ...env },
    },
  );
}

const hostSupported = Boolean(TRIPLE);
const skip = hostSupported
  ? false
  : `no platform package is published for ${HOST}`;

test(
  "forwards arguments, composes the child environment, and mirrors the exit code",
  { skip },
  (t) => {
    const root = stagePackage();
    t.after(() => cleanup(root));

    const result = runLauncher(root, ["-e", PROBE, "alpha", "beta"], {
      PROBE_EXIT: "7",
      // Pre-set so the launcher has something to clear: it promises exactly
      // one CODEX_MANAGED_BY_* is set, whatever the parent environment said.
      CODEX_MANAGED_BY_BUN: "1",
      CODEX_MANAGED_BY_PNPM: "1",
    });

    assert.equal(
      result.status,
      7,
      `expected the child's status to be mirrored\nstderr: ${result.stderr}`,
    );

    const report = JSON.parse(result.stdout.trim());
    assert.deepEqual(report.args, ["alpha", "beta"]);
    assert.equal(
      report.root,
      root,
      "CODEX_MANAGED_PACKAGE_ROOT is what the Rust side reads to find its own install",
    );
    assert.equal(report.npm, "1");
    assert.equal(report.pnpm, null, "a stale parent value must not survive");
    assert.equal(report.bun, null, "a stale parent value must not survive");
  },
);

test(
  "reports a missing platform package with something the user can act on",
  { skip },
  (t) => {
    const root = stagePackage({ withBinary: false });
    t.after(() => cleanup(root));

    const result = runLauncher(root, ["--version"]);

    assert.notEqual(result.status, 0, "a missing binary must not exit 0");
    assert.match(
      result.stderr,
      new RegExp(`Missing optional dependency ${PACKAGE_BY_TRIPLE[TRIPLE]}`),
    );
    assert.match(result.stderr, /codex-rewind@latest/);
  },
);

test("the platform table maps every triple to the package that carries it", () => {
  const source = fs.readFileSync(LAUNCHER, "utf8");

  // Names first: catches a rename, a dropped platform, or a seventh appearing.
  const named = new Set(
    [...source.matchAll(/"(codex-rewind-[a-z0-9-]+)"/g)].map((m) => m[1]),
  );
  assert.deepEqual(
    [...named].sort(),
    [...new Set(Object.values(PACKAGE_BY_TRIPLE))].sort(),
    "bin/codex.js must name exactly the six platform packages release.yml publishes",
  );

  // Then each pairing, which the name check above cannot see: swap two entries
  // in the table and the set of names is unchanged, while every user on both
  // platforms installs a package holding a binary their machine cannot run.
  for (const [triple, pkg] of Object.entries(PACKAGE_BY_TRIPLE)) {
    assert.match(
      source,
      new RegExp(`"${triple}"\\s*:\\s*"${pkg}"`),
      `bin/codex.js must map ${triple} to ${pkg}`,
    );
  }
});

for (const manager of ["pnpm", "vite-plus"]) {
  test(
    `recognizes ${manager} ownership of the rewind package`,
    { skip },
    (t) => {
      const staged = stagePackage();
      t.after(() => cleanup(staged));
      const base = fs.mkdtempSync(path.join(os.tmpdir(), "codexr-manager-"));
      t.after(() => cleanup(base));
      const packages = path.join(base, "packages");
      const install =
        manager === "pnpm"
          ? base
          : path.join(packages, "codex-rewind", "test-install");
      const modules = path.join(install, "node_modules");
      const root = path.join(modules, "codex-rewind");
      fs.mkdirSync(modules, { recursive: true });
      fs.cpSync(staged, root, { recursive: true });
      if (manager === "pnpm") {
        fs.writeFileSync(path.join(modules, ".modules.yaml"), "{}");
      } else {
        fs.writeFileSync(
          path.join(packages, "codex-rewind.json"),
          JSON.stringify({
            name: "codex-rewind",
            installId: "test-install",
          }),
        );
      }
      const result = runLauncher(root, ["-e", PROBE]);
      assert.equal(result.status, 0, result.stderr);
      const report = JSON.parse(result.stdout.trim());
      assert.equal(report.npm, null);
      assert.equal(report.pnpm, manager === "pnpm" ? "1" : null);
      assert.equal(report.vitePlus, manager === "vite-plus" ? "1" : null);
    },
  );
}
