# Verified file changes and Git evidence

> **Since v0.4.0.**

An agent hook saying "I edited `src/api.ts`" is a claim. For projects you
choose, TailFlow can also record what the filesystem and Git themselves
show, and compare the two. Nothing here identifies *who* changed a file.
A file watcher cannot see the writing process, and Git cannot tell an
agent's edit from yours.

## Turn it on

Observation is off by default. List the project directories to observe in
`tailflow.toml`:

```toml
[observe]
paths = ["."]                  # relative to tailflow.toml
exclude = ["fixtures/**"]      # extra globs, relative to each path
respect_gitignore = true       # default
git = true                     # Git snapshots for observed repositories (default)
max_directories = 20000        # Linux watch budget per path (default)
coalesce_ms = 200              # quiet time before a burst becomes one observation
git_min_interval_secs = 30     # at most one change-triggered snapshot per repo per 30 s
```

Or pass `--observe DIR` to `tailflow-daemon` (repeatable).
**Integrations → File & Git observation** and `GET /api/v1/observe/status`
show what is observed. (The startup banner's `observe` line can print
`0 directories` because it is written before the observers start; trust the
status endpoint.)

## What is observed

- **Only the listed directories.** TailFlow refuses `/`, your home
  directory itself, and anything inside its own data directory. Its database
  and data directory are never observed, even when they sit inside a
  project.
- **Not observed:**
  - `.git`, `node_modules`, `target`, `__pycache__`, `.venv`, `.next`,
    `.turbo`, `.gradle`, `.hg`, `.svn`, `.jj` and `.tailflow`;
  - editor swap and backup files (`*.swp`, `*~`, `.#*`, `*.tmp`, `4913`);
  - paths matched by `.gitignore` (the root file, `.git/info/exclude`, and
    nested `.gitignore` files present at start);
  - your `exclude` globs.

  Excluded changes are counted, not recorded, and are not loss.
- **Never contents.** An observation records the path, the kind of change
  (created, modified, deleted, renamed), the time, its size afterwards, and
  how many notifications were coalesced into it.
- **Symlinks are not followed.** A link inside the project that points
  elsewhere is not descended into.
- `[privacy]` boundaries apply as for every event (path globs,
  project allowlists).

Observations are `file.created`, `file.modified`, `file.deleted` and
`file.renamed` events with provenance `filesystem_observed` and
`source_type = "fs_watcher"`. Agent-reported ones have `agent_reported`.

| OS | Backend | Notes |
|---|---|---|
| macOS | FSEvents, recursive on the root | Paths under `/private/var/...` are mapped back to the form you configured |
| Linux | inotify, one watch per directory | At most `max_directories`; directories over the budget are counted and coverage is marked partial |
| Windows | ReadDirectoryChangesW, recursive | |

A burst of writes to one file becomes one observation after `coalesce_ms`
of quiet (at most 2 s after the first notification). An atomic save (write
a temporary file, rename it over the original) is one change to the real
path.

## Evidence statuses

**Files → Verified on disk** and `GET /api/v1/evidence/files` compare each
agent-reported change with observations of the same path within ± 5 s
(`window_ms`, up to 60 s):

| Status | Meaning |
|---|---|
| **Reported + observed** (`matched_observation`) | The agent reported the change and the watcher saw the path change within the window. An association, not proof of authorship: another writer could have changed it in the same window. |
| **Reported, not seen** (`reported_only`) | The path was watched with full coverage and no recorded loss, but no change was seen: a no-op write, a failed edit that was still reported, or a change outside the window. |
| **Unverified** (`unverified`) | The report could not be checked: nothing watched the path at that time, coverage was partial, or watcher loss overlaps the window. |
| **Observed, unreported** (`filesystem_observed`) | The watcher saw a change no agent reported: a person, a build or tool, or an agent without hooks. |
| **Ambiguous** (`ambiguous`) | One observed change matches reports from more than one session. |

**Coverage.** Each observed directory has one coverage record per daemon
run (start, stop, backend, directories over budget). Only one daemon
observes a directory at a time; a second daemon sharing the database waits
and retries every minute.

**Loss.** Loss is recorded as a gap and a `system.events.dropped` event,
under one of two reasons:
- `fs_observer_overflow`: TailFlow's queue was full; the count is exact;
- `fs_watcher_rescan`: the OS reported that it dropped notifications; the
  count is a lower bound.

The queue between the OS watcher and TailFlow holds 8,192 entries. A
create, modify or delete notification for a path that is already waiting is
merged into its entry (`coalesced` in the status) and takes no slot; renames
(each one may produce up to three notifications) and rescans each take a
slot. Access and attribute-only notifications (`filtered`), and changes
under the built-in excluded directories (`node_modules/`, `target/`, `.git/`,
…, counted in `excluded`), never enter it, so a build or a package install
does not crowd out real changes. Overflow therefore needs thousands of distinct
paths, or renames, changing faster than TailFlow processes them.
`/api/v1/observe/status` also reports raw OS `notifications` and the queue's
high-water mark (`queue_peak`).

## Git snapshots

For each Git repository that contains an observed directory, TailFlow takes
snapshots of the worktree:
- at startup;
- when an agent session starts or ends in that repository;
- after file changes settle, at most every `git_min_interval_secs`.

A snapshot records:
- `HEAD` and the branch;
- which paths differ from `HEAD`: modified, added, deleted, renamed,
  untracked or conflicted, staged or unstaged;
- for each path, its size, modification time and added/deleted line counts.

It never records file contents or diffs. It is a `git.snapshot` event. Up
to 150 paths are listed in it; the total is always recorded, and
`git.changes_complete` says whether the list is whole.

Git is only read:
- `git status --porcelain=v2` and `git diff HEAD --numstat` run with
  `--no-optional-locks`, so the index is not refreshed or locked;
- `core.fsmonitor` and the untracked cache are off;
- there is no external diff and no textconv;
- runs time out after 10 seconds and output is size-capped;
- TailFlow never stages, checks out, resets or cleans.

Filter drivers (Git LFS, for example) are disabled for snapshots: TailFlow
never runs a program a repository configures. If the configured filters
cannot be listed, the snapshot is skipped rather than risk running one. Files they manage may then
show as modified; the snapshot records `git.filters_disabled`.

A snapshot lists only paths under the observed directories (the total for
the whole worktree is still recorded), sorted by path. When the list is cut
(150 paths, or the 11 KiB budget), it stops at `git.changes_listed_through`,
and comparisons stop there too, so a path beyond the cut is never reported
as new.

When HEAD differs between two snapshots, TailFlow records one of:
- `git.checkout`: the branch changed;
- `git.commit`: a new commit whose parent is the previous HEAD, on the same
  branch;
- `git.head.moved`: anything else (reset, rebase, pull, amend).

Who moved HEAD is not known.

### A session's changes

**Session → Git changes** and `GET /api/v1/sessions/{id}/changes` compare
the snapshot taken when the session started with the one taken when it
ended. If either is missing, the closest snapshot is used instead, and the
response says so. Each changed path is listed with its evidence:

- **Reported by this session**: this session reported changing the path.
- **Reported by another session** / **Several sessions**.
- **Unreported**: no session reported it.

Paths already different from `HEAD` at the baseline that did not change
are listed as **pre-existing**, and are never credited to the session. The
response also lists:
- other sessions active in the repository;
- whether HEAD moved, since committed work no longer shows as a change;
- every limitation that applies.

## Command outcomes

**Commands → Tests, builds & checks**, **Session → Tools, files &
failures**, and `GET /api/v1/outcomes` classify each command an agent
reported by what it was for: build, test, lint, type check, format,
dependencies, other, or unclassified. Each classification comes with the
rule that decided it (`cargo test`, package script `test:unit`, `tsc` is a
typecheck tool).

In the default capture mode TailFlow does not store command lines. To tell
`cargo test` from `cargo build` it records a command's *intent* instead:
- the program that does the work (`cd web && npm test` → `npm`);
- its subcommand, only if it is in that tool's fixed vocabulary;
- a package-script name, only if it matches a fixed pattern (`test`,
  `build:prod`, `lint:fix` …).

An unknown word is recorded as "not recorded", never as the word itself.

Outcomes are only what the agent reported:

| Outcome | Meaning |
|---|---|
| `reported_success` | The agent reported completion (for Claude Code a failing exit fires a separate failure hook). |
| `reported_failure` | The agent reported a failure, with its exit code when given (Claude Code: parsed from the error text). |
| `interrupted` | The agent reported an interruption. |
| `outcome_unknown` | No outcome was reported (Codex without an exit code, a command still running, a missing hook). |
| `verified_exit_status` | Reserved for exits TailFlow observes itself. It never observes agents' commands, so it never appears for them. |

Runtime errors from a command's start to 30 seconds after its end are
listed as **nearby**: close in time, with no recorded relationship.
TailFlow does not parse command output or build logs.

## Related evidence

The event inspector (and `GET /api/v1/events/{seq}/related`) lists events
related to the selected one in five tiers, strongest first. Each item says
why it is listed:

| Tier | Linked by |
|---|---|
| 1. Direct identifiers | Operation or session ids the source reported (a tool call and its command) |
| 2. Independent observation | The same file change reported by an agent and seen by the file watcher within ± 5 s |
| 3. Shared context | The same session, close in time |
| 4. Temporal proximity | Runtime errors and warnings (or agent activity around a log line) within ± 30 s, with no recorded relationship |
| 5. Inference | One stated heuristic: a runtime error after a file change whose message contains the file's name. Possibly related, not confirmed |

None of these is a claim that one event caused another.

## Limitations

- A watcher is not a forensic record. The OS can coalesce or drop
  notifications (recorded as loss when the OS says so), and nothing is
  observed while the daemon is not running (no coverage).
- Observations do not identify the writing process. Two agents and a person
  editing one repository at once can only be told apart by their own
  reports.
- A session-start snapshot is taken when the daemon sees the session start
  (at once when this daemon recorded it, within about 2 seconds when another
  daemon sharing the database did). Changes in that first moment can appear as
  session changes.
- `.gitignore` files created after the observer started apply from the next
  start. The ignore syntax supported is a subset; character classes such as
  `[[:alpha:]]` are not supported.
- Network filesystems, containers' bind mounts and some editors' safe-write
  modes may produce notifications late, coalesced or not at all.
