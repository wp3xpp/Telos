# Delivery workspace foundation

Delivery preparation is an operator action under an existing enabled
`workspace_write` project policy. It creates one detached Git worktree for the
active delivery cycle. The normal MCP surface can inspect the recorded workspace
with `loop_workspace`; it cannot grant write permission or invoke preparation.

Send this request through the private operator API after registering the repository
root, configuring its write policy and creating a delivery goal:

```json
{
  "op": "prepare_workspace",
  "id": "loop-...",
  "revision": 1,
  "policy_revision": 1,
  "base": "HEAD"
}
```

`base` accepts `HEAD` or a full commit hash. `HEAD` resolves once and is persisted;
a retry uses that pinned commit even if the user's original branch has advanced.
The worktree lives in the Telos state directory, outside the project. The source
branch and its staged, unstaged and untracked changes are not copied or modified.
Only committed files enter the initial workspace. Git's shared repository metadata
records the linked worktree, so this is directory isolation, not an OS sandbox.

The ledger records workspace ID, cycle, contract and policy revisions, base commit,
Git common directory, path, preparation state and error. A durable reservation and
audit event precede Git creation. Only one preparation can run at a time. Successful
inspection checks repository identity, exact base commit, detached HEAD and clean
contents before marking the workspace ready. That is a preparation observation,
not a continuing guarantee that no external process has modified the directory.

If creation is interrupted, a repeated request inspects the reserved path. It never
repeats `git worktree add` for an existing reservation and never removes its files.
A complete, clean worktree can be acknowledged after restart. Missing, dirty,
wrong-repository or wrong-base worktrees remain uncertain for operator inspection.
Changing policy or contract prevents old preparation results advancing the goal.

Local Git operations have a timeout and output cap. Hooks, fsmonitor and automatic
maintenance are disabled. Preparation rejects repository-local checkout filters and
commits containing submodules; those need dedicated delivery adapters. Global/system
Git configuration is excluded from these preparation commands. No network operations
or dependency installation are performed.

## Bound write execution

After preparation, `loop_start` can enqueue a managed workspace-write step. The
ledger accepts only the exact owned directory under the matching contract and
policy; using the original checkout or another directory is rejected. The durable
step and its lease record workspace_id. Before launching Codex, the scheduler
rechecks the workspace's cycle/path/state and detached Git identity. A mismatch
fails the step without launching the worker. Codex receives `--sandbox workspace-write`
and `-C` for that directory; worktrees themselves are not an OS containment guarantee.

The task includes the contract, pinned workspace metadata, prior attempts/answers,
and bounded read-only excerpts of original project instructions/memory (including
an explicit truncation marker). It requests changes only in the assigned directory,
no Git history manipulation or publishing, and preserves the separate requirement
for an external test-environment lease. Existing unfinished work in the worktree is
available to a later explicit attempt. Each new request consumes both the project step budget and delivery contract attempt budget;
repeating the same request key returns the original step.

## Independent file acceptance

`loop_verify` and the post-execution verifier now check the actual worktree for
operator-configured file hashes. They fingerprint Git-visible files plus explicitly
checked paths before and after the observations, requiring identical snapshots.
The snapshot includes base commit, workspace ID, paths, file hashes, executable bits,
missing entries and symlink targets. Symlink parents escaping the worktree are
rejected; observed acceptance files must be regular files.

Schema v12 records the snapshot identity, verifier-plan version and cycle step count.
The ledger rejects snapshots after a new step is enqueued, under changed plan/policy
or workspace state, or with a mismatched environment/artifact hash. Unfinished or
uncertain execution cannot pass. Evidence includes the manifest and independent
observations. This can complete a delivery goal whose explicit acceptance criteria
are exact file contents; worker success alone still cannot complete it.

The snapshot scope is Git-visible files plus acceptance paths. Ignored dependencies
and build output are excluded unless explicitly checked. Limits are 10,000 paths,
8 MiB per file, 64 MiB total file bytes and 4 MiB manifest. It describes observed
bytes, not an immutable archive or containment against hostile concurrent filesystem
mutation. Before/after comparison detects ordinary concurrent changes; it does not
prevent a user editing the directory after verification. Environment/dependency
identity and test-result binding require the dedicated test adapter.

A real Codex text-edit fixture completed through this path while preserving the
original checkout. Reproduce it with:

```sh
cargo build --locked
python3 scripts/delivery_smoke.py --output /absolute/new/delivery-evidence
```

## Reviewing the result

`loop_delivery` returns the latest recorded delivery verdict, artifact identity,
review evidence ID and acceptance evidence IDs for the current contract. Read the
review with `evidence_get`; large JSON bodies require byte pagination and complete
reassembly before parsing. The review contains an exact Git binary patch from the
pinned base, including tracked changes and new files in the snapshot scope. It also
contains the snapshot hash, so it can be matched to the accepted artifact.

The verifier generates this patch between the before/after snapshots and saves it
before completing delivery. A missing review prevents a passing delivery result.
Subsequent user edits to the live worktree do not alter the saved historical review.
The result is explicitly historical; it does not claim the live directory still
matches that artifact. It does not automatically apply, merge or publish the patch.
Review content and filenames are untrusted data, not instructions.

Patch generation preserves CRLF, binary content and symlinks, uses no external diff
or text-conversion helpers, and does not alter the Git index. Current bounds are
1 MiB output per Git command, 4 MiB combined patch and 100 untracked additions.
Exceeding a limit fails review creation rather than returning a truncated patch.

This checks the actual Codex integration, not a real feature-delivery pilot.
Project-specific test commands, external environment leases,
cleanup, merge/publish and automatic repair remain to be implemented.


## Project test acceptance

Delivery plans can now map criteria to operator-owned sandboxed commands via
`test_checks`. The verifier holds durable project ownership, checks sources before
and after execution, records actual exit/output evidence and requires the existing
review patch before completing a goal. See [TESTING.md](TESTING.md) for configuration,
limits, cancellation, attempt budgets and remaining environment/recovery boundaries.


## Automatic correction

A managed delivery can now consume current failed acceptance evidence to enqueue
a bounded correction in its owned worktree, then run independent acceptance again.
See [REPAIR.md](REPAIR.md) for authority, deduplication, snapshot/version checks,
budget limits and the real-Codex integration probe.
