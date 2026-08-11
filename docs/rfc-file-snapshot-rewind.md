# RFC: A Cross-Surface File Snapshot & Rewind Subsystem for Codex (git-free)

- **Status**: Draft for discussion
- **Demand**: [#9203](https://github.com/openai/codex/issues/9203) (373 👍, "Please make /undo back"), [#11626](https://github.com/openai/codex/issues/11626) (196 👍, "/rewind restoring both chat and code"), [#2788](https://github.com/openai/codex/issues/2788), [#3585](https://github.com/openai/codex/issues/3585), [#19205](https://github.com/openai/codex/issues/19205) ("Undo should never depend on Git"), [#22100](https://github.com/openai/codex/issues/22100), [#27636](https://github.com/openai/codex/issues/27636)
- **Cross-surface pain this addresses**: [#29388](https://github.com/openai/codex/issues/29388) (Desktop checkpoint blobs: 102 GB, no GC), [#28241](https://github.com/openai/codex/issues/28241) (Desktop turn-diff refs break libgit2 clients), [#4535](https://github.com/openai/codex/issues/4535) / [#15367](https://github.com/openai/codex/issues/15367) / [#2998](https://github.com/openai/codex/issues/2998) (IDE undo toolbar scope & reliability)
- **History**: #8214 (undo data-loss incident), PR #8424 (un-ship undo), PR #19481 (remove ghost snapshots)
- **Scope**: `codex-rs` (core + app-server capability; TUI as first consumer)

---

## 1. Summary

Codex today has **three divergent, each-partially-broken checkpoint mechanisms across its surfaces** — and none of them lets a user rewind conversation and workspace state together. This RFC proposes one **opt-in, git-free file snapshot subsystem** in `codex-rs` core, exposed through the app-server protocol, that every surface can consume:

- the **CLI/TUI** backtrack flow (double-Esc) gains "also restore files" — the tracker's highest-voted open request (#9203, #11626);
- the **IDE extension** gains a reliable substrate for its undo/changes toolbar, covering shell-made edits its current edit-tool-scoped tracking misses (#4535, #15367);
- **Desktop** gains a storage backend that does not write unbounded git objects into the user's own repository (#29388, #28241).

Snapshots are captured incrementally at turn boundaries and before tool executions, stored in a content-addressed store under `CODEX_HOME` (never in the user's repo, never touching the user's git state), scoped by a dedicated versioned ignore file, and garbage-collected by reference counting tied to session lifetime.

The design synthesizes what works in peer tools and answers, point by point, the failure modes that killed Codex's own ghost-commit feature (PR #19481):

| | Claude Code `file-history` | opencode shadow-git | Codex ghost commits (removed) | Codex Desktop turn-diffs (today) | **This proposal** |
|---|---|---|---|---|---|
| Storage | plain per-file copies | shadow git repo | dangling commits in the **user's** repo | git objects in the **user's** repo | content-addressed store in `CODEX_HOME` |
| Requires git | no | yes | yes | yes | **no** |
| Touches user git state | no | no | **yes** (object DB) | **yes** (refs + objects; breaks libgit2 clients, #28241) | **no** |
| Coverage | only tool-edited files | whole worktree | whole repo | whole tree per capture | workspace + tool-edited files outside it |
| Scope control | none | `.gitignore` (inflexible) | `.gitignore` + hardcoded lists | none (#29388: databases/models snapshotted) | **dedicated versioned ignore file** |
| Change detection | mtime/content per file | git index stat cache | **throwaway temp index (no cache)** | full-tree blob writes | own persistent stat cache |
| GC | 100-snapshot cap | `git gc` 7-day prune | n/a | **none — 102 GB incident** | refcount, tied to session lifetime |
| Redo | no | yes | no | n/a | yes (fork model + safety checkpoint) |
| Conversation integration | yes | yes | **no (file-only)** | no | yes (backtrack/fork integration) |

## 2. Motivation

### 2.1 The demand is the top of the tracker

#9203 (373 👍) asks for `/undo` back; #11626 (196 👍) asks for exactly this feature — checkpoint restore covering both chat context and Codex-applied edits. The motivating incidents are consistent: Codex deletes or overwrites files that are untracked or uncommitted in git, and the user has no recourse.

### 2.2 Three surfaces, three divergent mechanisms, all partially broken

| Surface | Today | Failure mode |
|---|---|---|
| CLI/TUI | conversation-only backtrack (fork) | files stay at latest state → *old-context/new-files mismatch* (#22100): the model reasons from a conversation state that predates files it sees on disk |
| Desktop | full-tree git-object checkpoints under `refs/codex/turn-diffs/` **inside the user's repo** | no scope control, no GC: 102 GB of orphan objects on a 5.7 GB project (#29388); non-standard refs break libgit2-based tools (#28241) |
| IDE extension | edit-tool-scoped "View changes / Undo" toolbar | toolbar silently disappears when the model edits via shell (`cat`/`python`) instead of the edit tool (#4535); unreliable undo (#3567, #3104, #15367) |

The team has acknowledged this is an active product area — on #4535: *"we're in the process of trying to make this more robust through both model and product work"*, and on #2998: *"We will also be iterating on this surface over time."* This RFC is intended as concrete input to that work: the three surfaces are each missing the same underlying capability, and each has independently grown a partial, mutually inconsistent substitute. That is precisely the "consistency across all Codex surfaces" concern from `docs/contributing.md` — as an argument **for** building the capability once, in core.

Notably, the #29388 reporter's own suggested fix ("store checkpoints in a separate repository under `~/.codex/`") converges on the storage model proposed here.

### 2.3 Git is not an acceptable foundation for this

A safety net that only works for git users, only inside repos, and only for git-visible files fails precisely the users who need it most (#19205; the #9203 incidents are about *untracked* files). Worse, both git-based attempts so far entangled Codex with user-owned git state: ghost commits wrote to the user's object DB and the restore path caused the #8214 index-clobbering data loss; Desktop's turn-diff refs bloat the user's repo and break third-party tooling. The lesson is not "be more careful with git" — it is that **session history belongs to Codex's own state directory, not the user's repository**.

## 3. Post-mortem of ghost commits, and how this design answers it

The removed feature (`Feature::GhostCommit`, key `"undo"`) died of three specific causes. Each maps to a design decision here:

### Death cause 1: it manipulated user-owned git state
Ghost commits wrote dangling commit objects into the user's real repository and restore touched the user's index. Two days after the #8214 data-loss fix, the feature was un-shipped (PR #8424).

> **Answer**: zero interaction with the user's git state. Snapshots live entirely under `CODEX_HOME/file_snapshots/`. Restore writes file contents only; it never runs git, never touches `.git`. (This also resolves the #29388/#28241 class of problems for any surface that adopts the subsystem.)

### Death cause 2: snapshot latency
Each snapshot ran `git status --untracked-files=all` (full tree walk) plus a **throwaway temporary index** (`GIT_INDEX_FILE` in a tempdir: `read-tree HEAD` → `add --all` → `write-tree`) — deliberately avoiding the user's index for safety, and thereby forfeiting git's stat cache. Every snapshot re-hashed changed files from scratch. The resulting slowness forced async execution, a mutating-tool readiness gate, a 240-second watchdog, and hardcoded exclusion lists (10 MiB files, 200-file dirs, `node_modules`/`.venv`/…).

> **Answer**: a **persistent stat cache owned by the snapshot subsystem** — the manifest records `(path, size, mtime, content_hash)`; a checkpoint stats the tracked set and re-hashes only entries whose stat fingerprint changed. This is exactly the mechanism that makes `git status` fast, reclaimed without the safety conflict that forbade ghost commits from using it. Combined with a bounded tracked set (§6.2), checkpoints are a fast stat-walk and can run synchronously — eliminating the gate/watchdog complexity entirely.

### Death cause 3: untracked-file bookkeeping
Because ghost snapshots were *partial* (size/dir exclusions), restore needed fragile `preexisting_untracked_files/dirs` bookkeeping to know which files it could safely delete.

> **Answer**: three structural rules replace the bookkeeping (§7): full rescan at every checkpoint (the manifest *is* the existence inventory), a **safety checkpoint immediately before any restore** (deletion is always recoverable), and a **witnessed-birth deletion rule** (only delete files whose creation the snapshot system actually observed).

## 4. Architectural fit: a core capability, client-consumed

Codex's protocol has twice codified that history rewind does not revert files — `Op::ThreadRollback` and the v2 `thread/rollback` docs both state: *"Clients are responsible for undoing any edits on disk."* This RFC does **not** propose reversing that division of responsibility. It proposes giving clients the shared infrastructure the doctrine currently assumes but no surface actually has:

- **Core** owns capture (it is the only layer that sees tool execution and turn boundaries) and the snapshot store.
- **Clients decide** when and whether to restore. The TUI's backtrack confirmation is the first consumer; the app-server protocol exposes the capability (a `restoreFiles` opt-in on `thread/fork`, plus a small query surface for "what snapshots exist for this thread") so the IDE extension and Desktop can consume the same substrate instead of maintaining their own divergent mechanisms.
- Nothing changes for clients that ignore the capability; the feature is opt-in at both the config level and the per-restore level.

This inverts the current situation — where the doctrine says "clients are responsible" but clients have nothing to be responsible *with* — without moving the responsibility boundary.

## 5. Goals and non-goals

**Goals**
1. One snapshot subsystem usable by every surface; TUI backtrack is the first consumer (restore workspace files and conversation together).
2. No git dependency; works in non-git directories; never touches user git state.
3. User-controlled tracking scope via a dedicated, versioned ignore file.
4. Cover files modified by shell commands (within the workspace) via checkpoint rescans; cover tool edits outside the workspace via track-on-edit.
5. Redo: no restore is ever irreversible.
6. Opt-in, with disk usage visible and a clean degradation story when off.

**Non-goals (v1)**
- Restoring files modified by shell/MCP *outside* the workspace scope (unobservable).
- Remote/cloud environments (`RemoteFileSystem`/`PathUri` paths) — v1 is local-only.
- Migrating Desktop/IDE onto the subsystem — v1 only ensures the capability is exposed where they could adopt it.
- Sub-file delta storage (chunking). The blob store interface is designed so content-defined chunking (FastCDC, as in restic/borg and Hugging Face's Xet) can be added later without changing callers.
- A standalone `/undo` that restores files without rewinding the conversation. Rewind is offered through backtrack (Esc) and `/rewind`, both of which branch the conversation as well.

## 6. Design

### 6.1 Storage: content-addressed store + manifests + stat cache

```
CODEX_HOME/file_snapshots/
  blobs/<xx>/<hash>          # content-addressed file contents (dedup for free)
  manifests/<manifest-id>    # one per checkpoint: [(path, mode, size, mtime, hash)]
  refs/<thread-id>           # thread → list of (turn_id, manifest-id) + refcounts
  turns/<turn-id>            # turn → manifest-id, global and branch-independent
  restores/<thread-id>       # thread → the undo records it may spend
```

- **Checkpoint** = stat-walk of the tracked set; entries whose `(size, mtime)` match the previous manifest reuse its hash (no read); changed/new entries are hashed and their blobs stored if absent. Result: a new manifest (itself content-addressed — identical states dedup to one manifest).
- **A fingerprint recorded too soon after a write is marked untrustworthy for good.** A second write landing in the same timestamp tick as the read that preceded it leaves `(size, mtime)` unchanged forever after, so the cache would reuse a hash describing the earlier bytes — git calls this "racily clean", and here it is routine rather than exotic: an agent edit and the checkpoint that follows it land milliseconds apart. Note the hash cannot simply join the fingerprint; the fingerprint exists so the file need not be read, and the hash is the value being cached, not part of the key (git splits them the same way). So, as in git, raciness is decided once at capture time and recorded in the entry: a file captured within the racy window of its own mtime stores a zeroed fingerprint that never matches, and is therefore re-read until some later capture finds it settled. Deciding this at *lookup* time instead would only cover the first seconds after a write.
- Large-file copies attempt reflink (`copy_file_range` / FICLONE) first, falling back to plain copy — O(1) large-file snapshots on btrfs/XFS/APFS.
- Naming note: "snapshot" is already overloaded in this codebase (`shell_snapshot` feature, insta test snapshots). The crate and feature use **`file_snapshots`** consistently.

### 6.2 Scope: workspace resolution + seed tracking + ignore file

**Workspace root resolution** (git-style walk-up, reusing `project_root_markers` config and `codex-file-system/src/find_up.rs`):
1. Walk up from cwd looking for a project marker. If found → workspace mode: the tracked set is the workspace subtree, filtered by the ignore file.
2. No marker → **fallback mode** with seeded tracking: if the directory holds ≤ 50 eligible files, track all; otherwise seed with the 30 most recently modified. The set then grows via track-on-edit, hard-capped at **100 tracked files**. (These guardrails replace the old hardcoded exclusion lists — activity-based rather than attribute-based. They are also what Desktop's mechanism is missing when it snapshots databases and model weights, #29388.)

**Hidden entries are out of scope by default.** Scans skip dot-files and dot-directories, and `.git` unconditionally. Hidden paths are overwhelmingly tool state — editor settings, virtualenvs, caches, `.env` credentials — and silently rolling those back with a turn would be a nasty surprise, occasionally a destructive one. Work product that happens to be hidden stays recoverable through the edit hook: a `.github/workflows/ci.yml` the agent modifies is tracked from that edit onward. The `[file_snapshots] track_hidden_files` knob opts back in; `.git` remains excluded even then. The snapshot ignore file follows the same rule as any other dot-file, so it is versioned only once the agent edits it.

**Dedicated ignore file** (gitignore syntax via the `ignore` crate, already a workspace dependency — parse as in `file-search/src/lib.rs:370-384`):
- Separate from `.gitignore` by design: session history may legitimately track files git ignores (scratch notes, generated docs) — and vice versa.
- **Symmetric semantics**: an ignored path is never snapshotted, never restored, and **never deleted** by a restore. Ignore means invisible in both directions. This one rule replaces the entire `preexisting_untracked` bookkeeping class of the old design.
- **Versioned like `.gitignore`**: the ignore file itself is tracked. At restore time, the *current* ignore file governs protection; if the restore replaces the ignore file, subsequent operations follow the restored rules. Rules never change mid-operation.

**Outside the workspace**: track-on-edit — files the agent's own tools modify are tracked by absolute path, captured from pre-images already available in the apply-patch pipeline (§6.3). Creations are recorded as tombstones, so a path that only ever existed outside the scanned scope can still be removed by a restore that predates it. The bulk rescan never leaves the workspace.

**Scope follows the session's `workspace_roots`.** Resolving the root by walking up for `.git` answers where version control begins, not what the user is working on, and the two diverge in both directions — a monorepo puts the marker far above the actual work (the #29388 over-capture shape), while a non-repo directory falls back to the invocation directory, which may sit *below* where the agent is editing. Codex already carries a first-class answer: `workspace_roots` is multi-root, user-configured, and is what the sandbox consults to decide where the agent may write. Aligning the two makes "everything the agent can change can be reverted" structural rather than coincidental, and it is precisely the shell-made edits — the ones only a scan can catch — that occur wherever the sandbox permits. The marker walk-up survives only as a last resort, for when nothing is configured.

Two consequences fall out. The scope is now **plural**, so a capture records the roots it walked rather than a single `complete` bit: completeness is a claim about what was enumerated, and a manifest also carries paths the edit hook picked up from anywhere on disk. Without the roots, absence of one of those from a later capture would read as proof of deletion and remove a file that plainly existed. Recording them also lets the session's roots change mid-session without invalidating earlier manifests, since each states what it actually covered.

And configured roots are **filtered against the turn's cwd**: a root that neither contains nor sits under the directory being worked in is dropped. Configuration can name a different environment or simply be stale, and scanning an unrelated tree is the over-capture failure this design exists to avoid — the same shape as #29388, arrived at from the opposite direction.

### 6.3 Capture points

| When | Mechanism | Cost |
|---|---|---|
| Turn start (each user message) | full checkpoint of tracked set, in `run_turn` next to `TurnDiffTracker` instantiation (`core/src/session/turn.rs:~256`) | stat-walk |
| Before each tool execution (incl. shell) | incremental checkpoint | stat-walk |
| Per structured edit | **free pre-images**: `AppliedPatchDelta` already carries pre-edit content keyed by absolute path (`apply-patch/src/lib.rs:224-245`); persist it where `track_delta` is invoked (`core/src/tools/events.rs:625-654`) via a store handle sibling to `SharedTurnDiffTracker` in `ToolInvocation` (`tools/context.rs:59-71`). Fallback to an eager read when `delta.is_exact() == false`. | ~zero |

Note the continuity with existing infrastructure: `TurnDiffTracker` already retains per-turn pre-images in memory (`baseline_by_path`) for diff display; this subsystem persists the same class of data durably, and closes the gap `TurnDiffTracker` deliberately leaves open (shell-made edits, which it invalidates on) via checkpoint rescans — the same gap that hides the IDE extension's undo toolbar (#4535).

Shell/MCP/`write_stdin`/user-`!` edits inside the workspace are captured by the *next* checkpoint; outside the workspace they are out of scope (documented limitation, same as every peer tool).

### 6.4 Persistence: fully decoupled from the rollout

The rollout format is **not modified at all**. The snapshot subsystem keeps
its own per-thread log (`refs/<thread-id>` — an ordered list of
`{turn_id, manifest_id}` pairs) and references the rollout's existing turn
ids; the rollout never references the snapshot store. The join key is the
`turn_id` already present on `TurnStarted` items, stamped onto each
checkpoint at capture time. "The state at turn N" is by definition the
checkpoint captured at turn N's start.

This inverted reference direction eliminates three hazards the embedded
alternative (a `TurnContextItem` field) would carry:
- no interaction with `TurnContextItem`'s whole-struct dedup
  (`core/src/session/mod.rs:3689`) or its compaction reference paths;
- nothing new persists in the rollout, so the legacy `ghost_snapshot`
  stripper (`rollout/src/recorder.rs:1004-1128`) is irrelevant;
- fork truncation cannot lose snapshot refs — they are not in the rollout;
  restore resolves `before_turn_id` directly against the snapshot store.

**Resolution is keyed on the turn, not on the thread.** A turn id is
minted once and copied verbatim into every branch that inherits the turn,
so `turns/<turn-id> → manifest-id` answers "what did the tree look like at
this point in the conversation" identically no matter which branch asks.
Keying on `(thread_id, turn_id)` instead would make the answer depend on
which branch the user happened to be standing in, and rewinding to the
same point twice could then resolve to two different states. The undo
record is keyed by thread, but by the **destination** — the branch a rewind
hands the workspace to — rather than by the thread that performed it. The
performer is the wrong key because a rewind leaves it behind; the user is
in the destination when they ask to undo. A workspace key is the other
wrong answer: it is reachable, but it is *shared*, so two sessions working
in one directory push onto the same stack and whichever undoes first spends
the other's record. The destination is the only key that is both reachable
and private.
The result is the property the feature actually needs — **restoring to a
given point is idempotent**, and repeating it is a no-op rather than a
second, different move.

**Session tracking state needs no rollout field either**: with
session-scoped binding (§9), the existence of a thread's snapshot log *is*
the persisted "tracking enabled" marker. It is written by the **first
checkpoint**, not at session start. Codex mints a thread id whenever the TUI
launches and abandons it if the user immediately resumes something else or
quits, which is why it writes the rollout lazily too — a session that never
ran leaves nothing behind. Marking eagerly made this subsystem the one
component that littered: on a development machine more than half the logs
were empty. Deferring costs no information, because the only question the
marker answers is whether snapshots exist, and a session with none has
nothing to resume tracking from either way. `resume` and fork consult the store.
(If other surfaces later need to see tracking state, expose a read-only
app-server query; still no rollout change.)

Consequences accepted: refs are advisory (restore validates manifest
existence and aborts cleanly before mutating anything if the target is
missing); an orphan sweep removes logs for threads whose sessions are
gone; a rollout file alone no longer carries rewind ability (snapshots
are host-local data regardless); and `turn_id` becomes the join contract
between the two stores.

### 6.5 Restore: backtrack integration

Hook: **app-server `thread_fork_inner`** (`app-server/src/request_processors/thread_processor.rs:4214-4296`) — the only place that simultaneously holds `before_turn_id`, the source rollout, cwd, and (post-fork) the new thread id. Gated by a new opt-in `ThreadForkParams` flag (e.g. `restoreFiles`), set by the TUI from the backtrack confirmation (`tui/src/app_backtrack.rs:187-204` → `event_dispatch.rs:284-362` → `app_server_session.rs:704-810`). Because the hook lives in app-server rather than the TUI, **any** protocol client (IDE, Desktop) gets the same capability for free.

Restore procedure for target turn N:
1. **Safety checkpoint** of the current tracked set → manifest S′ (this makes every subsequent step reversible, and is what enables redo).
2. Resolve turn N through the **global turn index** (§6.4). The index lives outside the rollout, so fork truncation cannot lose it (fork-before-N truncates strictly before `TurnStarted(N)`, `core/src/thread_rollout_truncation.rs:215-221` — irrelevant to an external ref). Refs are advisory: a missing manifest aborts the restore cleanly before any mutation.
3. Compare exactly two manifests — N and S′ — and restore content/mode wherever they differ (skipping paths matched by the *current* ignore file). Deliberately no history walk: the plan is a function of where the tree is and where it is going, which is what makes step 2's idempotence survive all the way to the filesystem.
4. Deletion pass — delete only on **positive evidence of non-existence at N**, of which there are two independent kinds. Manifest N was a **complete scope scan**, so absence is definitive; or manifest N carries an explicit **tombstone** for the path. The distinction matters because the first is a deduction from completeness and the second is a recorded observation: when the edit hook sees a path created (no pre-image to save), it writes down that the file did not exist, which is the only thing that can license removing it later outside the scanned scope. Without tombstones a bounded capture could never delete anything, so a fallback-mode rewind would undo edits but leave every created file behind — a half-rewind of exactly the kind §3 sets out to avoid. Files the system has never observed are still never deleted, and every deleted file is recoverable from the safety checkpoint by construction.
5. Append `{target: N, safety: S′}` to the restore log of the thread this hands the workspace to — the record `/redo` reads. A restore with no destination records nothing and is therefore not undoable; that case is confirmed with the user first (§6.5, first prompt).

**Rewinding to the very first prompt** has no earlier turn to branch from, so the conversation restarts rather than forking (existing TUI behaviour, #33201). The files still have somewhere to go: the first turn's own checkpoint is the state before the agent acted, which is precisely what the user asked for. A separate `thread/restoreFilesToTurn` request covers this — the file half of a rewind, for clients that are not forking. It shares target resolution, safety checkpoint, and undo record with the fork path, so both produce identical workspace state for the same turn. Without it the conversation resets while the workspace keeps every change: the old-context/new-files mismatch of §2.2, reintroduced at the one point where a user is most explicitly asking to start over.

This one case is **asymmetric, and is confirmed before it runs**. Every other rewind branches, and the branch is what `/redo` walks back along; a restarted conversation has no parent, so the conversation cannot be brought back automatically — only resumed from the archive by hand. The files are unaffected by this, since the undo record belongs to the workspace rather than the conversation. Rather than engineer around the asymmetry, the TUI states it: choosing the first prompt opens a confirmation naming exactly what is and is not recoverable. Making undo work here would mean an undo whose calling thread has no snapshots of its own — reachable, but it removes the one thing that currently keeps a snapshot-less thread away from a shared stack, at a point where that stack has no other attribution (§11). Stating the cost is the smaller change.

**Redo** falls out of the same machinery: file redo is "restore manifest S′", resolved from the workspace log rather than from any thread, so it works from whichever session the user is in.

The conversation is not copied. Undo returns to the thread the rewind superseded, which is archived rather than duplicated: archiving moves a rollout out of the sessions directory, and thread lookup only searches that directory, so an archived thread cannot be resumed or continued behind the feature's back. It is frozen by construction, which is what a copy would otherwise be buying.

Rebuilding the conversation from a stored copy was tried and reverted, because it breaks the property §6.4 rests on. **Turn ids are runtime identifiers and are not persisted in the rollout**, so a thread rebuilt from one is assigned fresh ids. Every snapshot on that line is keyed by the old ids, so the first rewind after an undo resolves nothing and silently restores no files at all — the conversation moves and the workspace does not. Keeping the thread's identity keeps the join key intact. The cost is that an undo cannot recover a conversation the user has since deleted outright; the files still restore, and the failure is reported rather than silently half-applied.

**Undo records are private, but the files are not.** Keying them by destination thread stops sessions from spending each other's, and nothing more: two sessions editing one directory still write to the same files, so an undo restores a state from before the other's work and replaces it. Bookkeeping cannot fix a genuinely shared resource — but it can refuse to be silent about it. A restore record names the state its rewind produced, so before undoing, the paths that restore would write are hashed and compared against it. Anything that no longer matches was changed by something else, and the user is asked before it is overwritten. Contents are hashed rather than compared by stat fingerprint: the fast path can miss a same-length rewrite inside one timestamp tick (§6.1), and a false "unchanged" here means overwriting someone's work, which is the failure being prevented.

**This check lives in the client, with the other two confirmations.** Whether to ask is a presentation decision, not a protocol one — a different surface may reasonably ask differently, or not at all, and the two existing confirmations (undoing past later turns; rewinding to the first prompt) are already decided entirely TUI-side. The alternative designs both push the judgement into the protocol: a preview method, or a `force` flag on the undo. Neither is warranted. The snapshot store is local and already a TUI dependency — `/status` reads it for disk usage — so the check costs a file read rather than a round trip, and the protocol keeps the shape it has. That matters more than usual here: `ClientRequest` is a closed upstream enum with no extension point, so every method is a permanent edit to a file upstream owns (§10).

Redo is offered, not assumed: it returns the workspace to the moment of the rewind, so any work done since goes with it. That work is never destroyed — the rollout is archived and its snapshots stay pinned by the turn index — but nothing leads a user back to it, so the loss is one of reachability rather than data. When turns have happened since the rewind, redo asks first and says how many it would discard.

The workspace log is a **stack**: a rewind pushes a record, an undo pops the one it reverses. This matters as soon as the user rewinds twice. If undo simply targeted "the last restore", the second undo would reverse the *first undo* — files oscillating between two states while the conversation kept stepping backwards, which is the worst kind of divergence because each individual step looks correct. Popping makes N undos retrace N rewinds, and the conversation side does the same thing by walking the fork parents.

**A rewind presents as one conversation, not as a branch.** Mechanically it is still a source-preserving fork — that is what keeps the pre-rewind rollout intact for redo — but the thread it supersedes is archived as the fork is adopted, so the session list shows a single continuing conversation instead of accumulating near-identical branches the user has to tell apart. `/redo` swaps them back. This is a client-side presentation choice built on the existing archive API; the protocol surface stays branch-shaped, so a client that *wants* to show branches still can.

A second restore surface — `thread/rollback` (`core/src/session/handlers.rs:451-553`) — can reuse steps 1–4 after `apply_rollout_reconstruction`; v1 may defer this (the API is deprecated in favor of `thread/fork`).

### 6.6 Known coverage gaps (documented, not hidden)

- Shell/MCP edits **outside** the workspace: unobservable (same as all peer tools). UX must state restores are scoped.
- Sub-agent threads: their edits are keyed to their own turn ids; restoring a parent turn should aggregate descendant-thread manifests spawned during that turn (`thread_id` vs `session_id`, `core/src/session/session.rs:495-501`). v1 may restrict backtrack-restore to threads without sub-agents and lift the restriction later.
- Steered mid-turn messages have no `TurnStarted` of their own; backtrack already refuses to fork at steer prompts (`app_backtrack.rs:552-553`) — consistent, documented.
- Remote environments: paths are `PathUri` via `RemoteFileSystem` — v1 skips capture/restore for non-local environments explicitly.
- Paginated-history threads take the `prepare_fork`/`ForkBoundary` path (`thread_processor.rs:4074-4105`); v1 excludes them explicitly.
- Code Mode needs no special handling, which is worth stating because the opposite is the natural assumption. Models in `code_mode` / `code_mode_only` appear to bypass the tool layer, but the sandbox is a tool *router*, not a second effector: the four code-mode crates contain no filesystem primitives at all, the TypeScript surface handed to the sandbox is generated from the tool descriptions themselves, and its only outward path is `DispatchMessage::InvokeTool` back into the very same `ToolCallRuntime` — constructed with the same `SharedTurnDiffTracker` (`core/src/tools/code_mode/delegate.rs:54`). The pre-image hook hangs off tool execution rather than off the model's direct call path, so nested edits capture identically, and track-on-edit keeps working for files outside the workspace. A future execution surface that writes files *without* going through a tool would break this, and is the thing to watch for.

## 7. Correctness rules (summary)

1. **Never touch user git state.** No exceptions.
2. **Safety checkpoint before every restore.** Deletion is therefore always recoverable.
3. **Definitive-absence deletion.** Delete only on positive evidence: absence from a target manifest that was a complete scope scan. Never delete a file whose existence at target time is uncertain.
7. **Restoring to a point is idempotent.** The same target resolves to the same state from any branch, and applying it twice changes nothing the second time.
4. **Symmetric ignore.** Ignored paths are never snapshotted, restored, or deleted.
5. **Current ignore wins at restore start**; a restored ignore file governs from the next operation on.
6. **Never-tracked files are never modified in any direction.**

## 8. Garbage collection

- Manifests reference blobs; refs reference manifests. Mark-and-sweep from three root sets: thread logs, the turn index, and the workspace restore logs.
- **Snapshot lifetime = session lifetime**, with one deliberate exception. Deleting a session drops its refs, and the turn index is pruned to the turns some live thread still holds, so its captures become garbage as before. But a workspace's most recent restore records stay reachable, because `/redo` has to survive the session that produced it — otherwise closing codex silently disarms the undo for a rewind the user just performed. That log is capped (20 records per workspace), so the exception is bounded rather than an open-ended retention window.
- No timers, no age-based sweeps (the `shell_snapshots` 3-day pattern is explicitly *not* copied — it would break restore for older forks; #29388 is the no-GC failure mode this rules out by construction).
- Forked threads share manifests with their source — cheap and correct.

## 9. Configuration & UX

- **Feature key `file_snapshots`** (new key; `Stage::Experimental` → appears in the `/experimental` menu automatically, CLI `--enable file_snapshots` works for free). The retired `"undo"` key is **not** reused: old configs in the wild still carry materialized `undo = <bool>` values that would silently re-bind (`features/src/lib.rs:488-490, 760-781`).
- **Config section `[file_snapshots]`** cloned from the `GhostSnapshotConfig` plumbing pattern (`config_toml.rs:733-744`) — but as a *new* struct: `GhostSnapshotConfig` is public API via `core-api/src/lib.rs:44` and stays untouched. It carries the fallback-mode seeding limits (`seed_full_limit`, `seed_recent`, `max_tracked_files`) and `track_hidden_files`; workspace mode needs no seeding.
- **Session-scoped binding**: the toggle answers one question — *do NEW sessions track?* Enabling does not affect existing sessions; disabling does not stop sessions already tracking. A session's snapshot chain is therefore always complete-from-turn-1 or absent, and the state is persisted with the session so `resume`/fork honor it. No mid-session partial states exist.
- **Disk disclosure** in the toggle description: enabling uses additional disk under `CODEX_HOME/file_snapshots/` (content-addressed, deduped).
- **Usage visibility**: `/status` shows the store's current size.
- **`/rewind` command**: lists the session's prompts, newest first, so a target can be picked directly. Esc-stepping remains for the common "undo the last turn" case, but it walks back one prompt per press and is undiscoverable on its own.
- **Discoverability hint**: when backtracking with the feature off, the confirmation shows one line — "file changes will not be restored (enable `file_snapshots` in /experimental)".
- Deliberately omitted (v1): size caps, prune commands, retention settings — refcount GC plus bounded tracking keeps growth in check; add knobs only if real usage demands them.

## 10. Phasing & effort

| Phase | Content | Est. |
|---|---|---|
| 1 | `codex-file-snapshots` crate: stat cache, blob store (reflink-aware), manifests, refcount GC — no changes to existing code | 1–2 wk |
| 2 | Capture wiring: turn-start + pre-tool checkpoints, `AppliedPatchDelta` pre-images, session metadata flag | 3–5 d |
| 3 | Restore integration: `restoreFiles` fork param (+ schema/bindings regen), restore procedure in `thread_fork_inner`, fork log inheritance, session-delete cleanup | 3–5 d |
| 4 | Config/feature key, TUI confirmation + hints + `/status` line, insta snapshot-test updates | 3–5 d |

Total ≈ 4–6 engineer-weeks for a solid v1 behind an experimental flag. A later phase can add the app-server query surface ("list snapshots for thread") that lets the IDE extension and Desktop adopt the substrate.

## 11. Open questions

1. Eviction policy when the 100-file cap is hit in fallback mode (proposal: evict least-recently-changed; never evict files touched by the agent's own edits).
2. Should turn-start checkpoints be per-user-message only, or also before every shell execution by default (finer intra-turn granularity at slightly higher stat-walk cost)?
3. Future: per-pattern attribute routing in the ignore file (`full` / `stub` / `ignore`, LFS-attributes-style) so large binaries can be tracked as metadata-only stubs instead of excluded — directly relevant to the #29388 "databases and model weights" case.
4. Future: content-defined chunking (FastCDC) behind the blob-store interface for long-lived histories.
5. Adoption path for Desktop/IDE: should the v1 app-server surface already include the snapshot-query API, or is the fork `restoreFiles` flag enough until a surface commits to adopting?

---

*Prepared with a full archaeology of the removed ghost-commit feature (commits e0fbc112c7 → 052b052832 → 014235f533 → 7a8407bbb6 → 4e05f3053c → f50c02d7bc → 862b2122ee) and function-level verification of every integration point cited above against current `main`.*
