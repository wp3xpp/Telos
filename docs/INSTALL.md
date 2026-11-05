# Development installation

Build with `cargo build --locked`. Install using explicit absolute paths:

```sh
target/debug/telos --state-dir /absolute/telos-state install \
  --prefix /absolute/new-telos-install \
  --codex-home /absolute/existing-codex-home \
  --codex /absolute/codex
```

The prefix must not already exist; its parent and the Codex home must exist. State
must remain outside removable installation paths. The command copies the binary,
installs the embedded hsw-telos Skill, and registers a stdio server named `telos`
through `codex mcp add`. Existing Telos registrations or skills are never overwritten.
Only this installation's known files are cleaned after a registration failure. An
uncertain configuration result retains files for inspection rather than deleting a
possibly referenced binary.

The returned `daemon_command` starts the service in the foreground. Use that exact
state directory for all commands. Installation does not start it automatically;
use the explicit service commands below for launchd management. Closing an unmanaged
terminal can stop the foreground service. `doctor` checks reachability and login without reading
credential files. No registered project or execution permission is created by install.

After installation, verify tool discovery and a real `telos_status` call inside the
desktop Codex environment. A successful CLI config command or CLI MCP fixture is
not desktop acceptance. The current active chat may not expose newly installed tools.

Stop the daemon, then uninstall from the source build or installed executable:

```sh
target/debug/telos uninstall --prefix /absolute/telos-install \
  --codex-home /absolute/codex-home --codex /absolute/codex
```

Uninstall checks the recorded binary/Skill hashes and MCP registration. Modified
files or registrations are preserved with an error. It removes only owned files and
the Telos MCP entry, leaving the state directory and unrelated configuration intact.
It requires the daemon ownership lock to be free, even if the socket does not respond.
Stop the service before uninstalling; finish any journaled upgrade first.
For a managed installation, run `service remove` before uninstall or upgrade.
Both operations refuse a retained service ownership record.

## Upgrade

Stop the daemon, then run the **new source build** against the existing installation:

```sh
target/debug/telos upgrade --prefix /absolute/telos-install \
  --codex-home /absolute/codex-home --codex /absolute/codex
```

Upgrade verifies the installed file hashes and exact MCP registration, acquires the
same exclusive state lock used by the daemon, and replaces the binary, embedded
Skill and manifest. It does not change MCP configuration, grant project authority,
start the service, or migrate the database. Start the new daemon afterward to run
its transactional schema migrations. Older binaries reject newer schema versions;
this command does not implement database downgrade or perform backup or restore.
Create and verify an offline backup with the separate commands below before upgrading
nonempty state.

A durable `upgrade.json` records old/new hashes before any installed file changes.
Staging and replacements use synced files and atomic renames. If interrupted, rerun
the same command from the original new build: it resumes that pending upgrade,
accepting only the recorded old/new destination contents. If staging was already
complete, it can finish from staged files. The journal is removed after all file
hashes match and staging cleanup finishes. User edits, modified registration or
corrupt staging stop the operation for inspection; they are not silently discarded.
This is forward recovery of installation files, not rollback of application data.
Keep the daemon stopped until the upgrade succeeds.

## Local notifications

On macOS, pass `--notifications` to `telos daemon` to deliver local notifications.
Without it, the durable outbox remains available for later delivery and inspection
via `notification_status`. Normal continuous passes stay quiet. Changed verdicts,
delivery completion and actionable execution/schedule states can notify. Newer
unsent verification notices supersede older pending ones for the same goal.

The dispatcher leases each notification, retries failures with exponential delay,
and stops after five attempts. State and delivery outcomes are audited. Sending to
the OS and committing to SQLite cannot be one transaction: a crash between those
operations can produce a duplicate. A `sent` state confirms adapter success, not
visual display or reading; OS notification settings can suppress display. Messages
contain a goal identifier, a generic state summary and event number, not raw logs.
The current adapter uses a fixed AppleScript with arguments, never generated script
code. OS notification permissions determine whether a delivered notice is visible;
check notification settings when enabling this feature.

Reproduce install/uninstall tests against a fresh isolated Codex home:

```sh
python3 scripts/install_smoke.py --output /absolute/new/install-evidence
```

The test uses actual Codex configuration commands without running model work. It
checks unrelated config preservation, duplicate installation rejection, preservation
of modified skills, state retention, lock enforcement, bundle replacement, simulated
interrupted-upgrade recovery and cleanup after simulated registration failure.

## Foreground service

For a first connection check or a setup that depends on the current shell's
allowed environment, run the installed binary in the foreground:

```sh
/absolute/telos-install/telos --state-dir /absolute/telos-state daemon \
  --codex /absolute/codex
```

Use the same state directory recorded during installation. Keep this terminal
open; Ctrl+C stops the foreground service. If a managed service is already
installed, remove it using `service remove --prefix /absolute/telos-install`
before running the foreground daemon. Do not run two daemons against one state
directory. Stop the foreground daemon before installing the managed service.

## macOS per-user background service

After installing and stopping any foreground daemon:

```sh
target/debug/telos service install --prefix /absolute/telos-install --codex /absolute/codex
target/debug/telos service status --prefix /absolute/telos-install
target/debug/telos service remove --prefix /absolute/telos-install
```

Add `--notifications` to service install to enable the notification adapter. The
service uses an owned plist in the current user's Library/LaunchAgents, runs in
that user's GUI domain, and restarts on exit. Health requires a responding daemon
whose PID matches launchd. Install does not grant project execution rights.
The service sets HOME, CODEX_HOME and a fixed executable PATH; it does not copy
credentials or arbitrary shell environment variables.

Removal confirms launchd unloading and released daemon ownership before deleting
its plist/metadata; state and logs remain. Modified plists are preserved with an
error. Failed readiness leaves ownership for `service status` and `service remove`.
Remove the service before upgrade, then install it again after the upgrade.

Store durable state under `~/Library/Application Support/Telos/`. macOS may
restrict background access to protected folders; verify project access through
the running service. Do not use temporary storage for durable state.

Reproduce with a fresh isolated test directory outside Documents:

```sh
python3 scripts/service_smoke.py --output /private/tmp/telos-service-unique-test
```

Verify desktop integration by invoking `telos_status` inside your Codex chat.

## Offline backup and verification

The source build supports:

```sh
target/debug/telos --state-dir /absolute/telos-state backup create \
  --destination /absolute/new-backup-directory
target/debug/telos backup verify --backup /absolute/new-backup-directory
```

Remove the launchd service (or stop a foreground daemon) first. Backup acquires
both lifecycle and daemon locks; a failed health check alone does not authorize a
backup. It refuses existing destinations and destinations inside live state.
Reinstall the service when offline maintenance is finished.

The snapshot contains a standalone SQLite database built using VACUUM INTO,
including committed WAL data, plus regular state files such as local evidence.
Lock files and SQLite WAL/shared-memory files are excluded. Regular files use private permissions (0600, or 0700 when executable).
This is not an installation archive. Special files and unrelated symlinks are refused. Workspace links and generated
Codex bootstrap links under run codex-home/tmp directories are copied as link
metadata without following their targets. Failed creation retains an incomplete
directory for inspection; it must not be treated as a valid backup.

Verification compares the exact file inventory and SHA256 hashes and checks SQLite
integrity, foreign keys, schema and recorded project/goal counts. The manifest is
not signed; hashes detect changed content relative to that manifest, not malicious
replacement of the entire backup. External projects/worktrees, remote environments,
installation configuration and credentials are outside this snapshot. Stop any
external writers to state artifacts before backup; the daemon lock cannot control
unrelated processes.

The staged/manual recovery and activation workflow below is available. The source
build also provides explicit offline backup retention below; live evidence retention
and recovery of an already restored/activated lineage as another generation are not.
In particular, copying old queued work into live state could replay effects that
happened after the snapshot. A valid backup does not establish safe automatic
resumption.

## Retain recent offline backups

Offline backup pruning is an operator-only collection cleanup command.
First preview a dedicated backup collection directory:

```sh
target/debug/telos backup prune --directory /absolute/backups --keep 3
```

This preview is read-only. It examines immediate child directories, verifies each
backup's exact payload inventory and database integrity, and groups valid backups
by `source_state`. It retains the newest three **per source**, ordered by manifest
`created_at` descending, then name ascending for ties. At least one must be kept;
invalid backups do not count toward the retained minimum. Manifest dates and source
names are unsigned metadata, not authenticated creation/ownership evidence.

Inspect `plan.retained`, `plan.remove` and `plan.skipped`. Only directories whose
root contains exactly `manifest.json` and `state` can be removed. Symlink roots,
partial/corrupt backups, recovery images, live state and directories with extra
files are skipped. External workspace links inside a verified backup are removed
as links; their targets are not followed. The scan is bounded to 10,000 entries.
Apply the exact preview hash explicitly:

```sh
target/debug/telos backup prune --directory /absolute/backups --keep 3 \
  --expected-plan PLAN_SHA256
```

Stop other writers to this collection before applying. The retention lock serializes
Telos cleanup processes only; it cannot stop arbitrary external filesystem writers.
Apply regenerates the entire plan, rejects a changed hash, rechecks retained copies
before each removal, and revalidates each candidate before and after moving it to
an exclusively named quarantine directory. Deletion is permanent, not a recycle bin.
The command does not need to stop the source service because it operates only on
closed backups, not on live state or external projects.

A synced `.telos-retention-*.intent.json` records the exact plan before any move.
Per-backup deletion records and a final completion record are synced afterward.
These are filesystem maintenance records, not live goal-ledger events. The whole
batch is not atomic: failure may leave earlier deletions complete and a quarantine
partly removed. An intent without a completion record is not proof of deletion or
success. Reserved `.telos-retention-*` files/directories are never automatically
selected or resumed; retain them for operator inspection. Preview lists any
quarantine names under `plan.quarantines_requiring_review` without reading their
contents or following links. Apply refuses further cleanup while this list is
nonempty. Inspect and resolve the interrupted operation before requesting a new
preview; do not delete these directories merely to clear the guard. Do not blindly
replay an old plan after interruption. This command provides neither scheduled cleanup,
live evidence/goal retention nor cleanup of its own audit/quarantine history.

## Stage a recovery image

```sh
target/debug/telos backup stage-restore --backup /absolute/verified-backup \
  --destination /absolute/new-recovery-directory
```

This reconstructs the verified database and local files under `state/` inside a
new recovery directory. It rechecks copied bytes and database integrity before
atomic publication, refuses any existing destination, and leaves failed staging
for inspection. It does not overwrite the live installation or modify its ledger.

Both the recovery root and its state payload carry `restore-required.json`. The
new daemon refuses these directories before process cleanup, migrations, task
scheduling or notification dispatch. The separate payload layout also keeps old
run journals away from the recovery root. Use the matching new source build; older
installed versions do not implement this recovery gate. Never bypass it by deleting
markers or pointing older runtimes at the inner payload.

**Staging does not activate or resume the image.** Absolute evidence/workspace paths,
remote outcomes, queued work and notification history must be reconciled against
what happened after the backup. Staging preserves historical records; it does not
prove those records describe the current outside world or authorize repeated actions.
Use the reviewed, operator-gated activation workflow below; do not treat staging as
completed restoration.

## Inspect staged recovery before reconciliation

```sh
target/debug/telos backup inspect-restore --recovery /absolute/recovery-directory
```

Newly staged images now include their original backup manifest, so inspection does
not depend on the original archive remaining at its old path. Images staged by an
older build without that embedded manifest must be staged again from their backup.
Inspection validates matching root/payload markers, the manifest hash, the exact
payload inventory and database integrity. Local maintenance lock files are ignored;
they convey no evidence of execution success. This command currently supports
unchanged schemas 16 through 19 and does not migrate or alter the copied ledger.

The report lists goals, steps, verification leases, notifications, schedules,
projects, execution policy revisions, external workspaces and stored evidence paths.
It includes full counts and at most 100 records per category, with explicit truncation.
Evidence records use paths relative to state (`evidence/ID.bin`). The report
resolves these against the copied state directory without rewriting historical
records. Nonconforming paths are flagged and never read as external files.
No external paths, processes, credentials or services are inspected.

Snapshot risk counts highlight pending/ready/running/unknown steps, unresolved
verification ownership and notifications that might replay. Zero counts do not
authorize activation: operations after the snapshot may be absent from the ledger.
The output includes manifest and database hashes to bind later review to this image.
It does not release the recovery gate, clear unknown outcomes or grant permissions.

## Recovery preparation compatibility

Source schema 17 adds a durable recovery-preparation record. An internal ledger
transaction can pause nonterminal goals, disable schedules, mark unconfirmed work
and verification ownership unknown, mark workspaces uncertain and suppress old
pending/sending notifications. It preserves attempt budgets and commits an audit
event with the state changes. The same snapshot fingerprints are idempotent; a
different snapshot is rejected.

This transaction is now reachable through the offline operator command below.
It is not an MCP capability and does not authorize activation. The daemon refuses a prepared
database even if filesystem recovery markers were lost, before inspecting old
process journals or starting workers. Preparation is not activation or evidence
that external effects have been reconciled.

Older binaries reject newer databases; retain a verified offline backup before
upgrading an installation.

## Prepare a reviewed recovery image

Run `backup inspect-restore` first and use its exact `manifest_sha256` and
`database_sha256` values:

```sh
target/debug/telos backup prepare-restore \
  --recovery /absolute/recovery-directory \
  --expected-manifest MANIFEST_SHA256 \
  --expected-database DATABASE_SHA256
```

The command revalidates the reviewed source image and owns its maintenance locks.
It keeps `state/` unchanged, creates a separate candidate and invokes offline daemon
maintenance to prepare its ledger. No worker, process-journal cleanup, external
inspection or notification sender runs. The prepared result is written under
`prepared-state/state/`, with a hashed receipt and recovery gates.

Only a complete, verified candidate is published, using exclusive atomic rename.
Repeating the command validates and returns the same completed result. Modified
results are rejected. Interrupted candidates remain under `.prepare-*`; they are
never considered completed or automatically removed. A retry can prepare a new
copy from the unchanged source. Hashes and receipts are integrity records, not
signed attestations of external outcomes.

This command does not activate the result. Unknown execution, external workspace
state, absolute artifact paths and notification disposition still need explicit
reconciliation before operational use. Do not remove the recovery markers.
The command changes only the recovery candidate, not the installed service; use a
schema-19 build for this workflow.

## Evidence integrity during recovery review

Recovery inspection now checks every evidence row against the hashed local file
inventory: the exact supported relative path, recorded byte length (1–8 MiB) and
SHA256 must match. It reports `evidence_integrity_complete` and per-record outcomes.
Checks run in bounded query batches across the entire ledger; only display is capped
at 100 records. Preparation refuses an image with missing, mismatched or unsupported
evidence bindings, even if the archive itself matches its manifest.

Supported relative evidence paths do not need migration. Absolute paths inside
immutable run reports and Git workspace metadata require separate reconciliation. State-local worktrees may contain Git pointers to external repositories;
copying their files does not establish a usable Git worktree. Format 2 snapshots now preserve workspace symlinks and executable bits, but Git
repository linkage still needs reconciliation before a copied workspace can run.

## Workspace links and executable files

New backups use manifest format 2; reading format 1 backups remains supported.
Within real directories under `workspaces/<workspace-id>/`, the archive preserves
symbolic links as links and records their target bytes and hash. It never opens
or recursively follows the target, including absolute, dangling, cyclic or non-UTF8
targets. Generated Codex bootstrap links under runs/*/codex-home/tmp are likewise
preserved as metadata, including after run archival. Workspace-root pointers and
unrelated symlinks elsewhere in state remain rejected.
Changing a link target is detected by archive/receipt verification.

Executable regular files retain their executable bit with private 0700 permissions;
other regular files use 0600. Setuid/setgid/group/other permissions are not copied.
These rules apply through archive creation, recovery staging and preparation.
Preserved Git pointer files or links do not establish that an external repository
is available, unchanged, correctly registered or authorized for further execution.

## Explicit external workspace identity check

Ordinary recovery inspection reads only the copied ledger. To request a read-only
check of a specific candidate checkout and its recorded Git repository:

```sh
target/debug/telos backup check-workspace \
  --recovery /absolute/recovery-directory \
  --workspace-id WORKSPACE_ID \
  --candidate /absolute/candidate-checkout \
  --expected-manifest MANIFEST_SHA256 \
  --expected-database DATABASE_SHA256
```

The hashes come from `inspect-restore`; they are checked before touching the
candidate. The workspace ID selects the pinned repository, base commit and
revisions from the verified image. Git checks require the expected repository,
detached HEAD at the recorded base, the correct checkout root and a reciprocal
repository worktree backlink. A copied `.git` file still pointing to another
checkout's administrative entry is rejected.

The command does not repair Git metadata, register/adopt the candidate, compare
working files to historical acceptance, resolve unknown external operations or
remove any recovery gate. Git optional locks, hooks, fsmonitor, auto-GC and
automatic maintenance are disabled. Success proves only the observed Git identity;
it does not authorize execution or establish that the working files are correct.

## Project-level recovery holds

Source schema 18 adds a persistent hold for every project present when recovery
preparation runs. This covers projects without unfinished steps: an empty snapshot
queue cannot prove that nothing happened after the snapshot.

While held, ordinary resume, new-goal creation in that project, cycle creation,
execution configuration, work admission, verification and workspace mutation
cannot authorize further action. Queue claiming also excludes held projects.
Read access and stopping a goal remain available. Existing project grants and
attempt budgets are preserved; the hold is a separate recovery requirement.

Holds commit with preparation and its audit event. Migrating an already-prepared
v17 image backfills holds transactionally and records that migration once. No
model-facing or ordinary loop operation can clear them; project reconciliation
and activation require the explicit offline operator workflow below. The installed
service uses schema 19.

## Serve prepared recovery data for read-only review

```sh
target/debug/telos --state-dir /absolute/recovery/prepared-state/state \
  daemon --recovery-review
```

This explicit mode opens a closed prepared schema 18 database read-only and
immutable, without migrations, WAL replay, process cleanup, workers, observers or
notifications. It uses the normal private control socket and stdio MCP bridge:

```sh
target/debug/telos --state-dir /absolute/recovery/prepared-state/state mcp
```

Point a separate temporary MCP client at this command to review the image; these
commands do not replace the active desktop registration. `telos_status` reports
`mode: recovery_review` and `execution_enabled: false`. Project listings include
recovery holds. Goal records, paginated events, local evidence and status queries
work. Modifications, acceptance checks, execution and external project/context
inspection are rejected before dispatch. `--notifications` conflicts with this mode.

The SQLite connection independently refuses writes. The service never interprets
old process journals or advances goal state. Socket/lock files are normal control
artifacts; the database remains byte-for-byte unchanged. Unprepared databases,
older schemas and SQLite sidecars are rejected. This mode makes recovery data
reviewable; it does not release project holds or activate restored execution.

## Offline project review

The source ledger now supports a separate trusted-operator review decision. It
requires operator attribution, an explicit statement that external state was
reviewed, the exact snapshot hash and 1–16 immutable evidence references. Evidence
is uploaded after preparation and bound to the project, snapshot and decision ID;
its bytes are rechecked before releasing a hold. The audit log indexes these
project-level materials, so a project need not have a nonterminal goal to hold them.

This is an attributed operator decision, not an automatic assertion that the
material is factually correct or that tasks succeeded. Pending/running/unknown
steps and running/unknown verification ownership must be resolved separately;
review cannot clear them. A successful review clears only the project hold, leaving
goals paused, budgets unchanged, workspaces uncertain where applicable and global
activation gates intact. The decision and hold change commit with an audit event.

The trusted offline command writes a **new** image and leaves its source intact:

```sh
target/debug/telos backup review-project \
  --image /absolute/recovery/prepared-state \
  --expected-receipt RECEIPT_SHA256 \
  --decision /absolute/operator-decision.json \
  --materials /absolute/operator-materials \
  --destination /absolute/reviewed-image
```

`RECEIPT_SHA256` pins the exact bytes of the source image's `receipt.json`.
The destination must not exist and must be outside the source image. Stop any
read-only review daemon on that source before invoking maintenance. Supply an
explicit decision (replace all placeholders with independently reviewed facts):

```json
{
  "project_id": "PROJECT_ID",
  "manifest_sha256": "BACKUP_MANIFEST_SHA256",
  "decision_id": "operator-review-001",
  "operator": "OPERATOR_ATTRIBUTION",
  "statement": "DESCRIPTION_OF_ACTUAL_EXTERNAL_REVIEW",
  "reviewed_external_state": true,
  "evidence": [{"id": "report.txt", "sha256": "REPORT_BYTES_SHA256"}]
}
```

Evidence IDs here are filenames directly within `--materials`; regular files only,
no links or path traversal, 1–16 materials each of 1 byte to 8 MiB. Only provide
intended review reports, never credentials. The command checks hashes before
copying, imports bytes through runtime maintenance, then replaces filename IDs
with generated immutable evidence IDs in the stored proof. Attribution is not
identity authentication, and these materials are not independently proven facts.

The output includes the new image location and receipt SHA256. Use those as
`--image` and `--expected-receipt` for another project's review; previous project
proofs remain in the ledger. Existing destinations are never overwritten. A
failed operation may retain a private `.review-*` candidate for diagnosis; it
remains gated and is never automatically reused. Retry from the verified source.
A published receipt is checked against both the file inventory and project proof.

No MCP tool or read-only review request exposes this authority. Both global
activation gates remain, even after every project hold clears. Final activation,
external-outcome settlement and workspace adoption use the separate commands below.
Installed bundles do not gain this command until upgraded.


### Reading recorded review material in Codex

On a reviewed image opened with `daemon --recovery-review`, call `project_list`.
Each `recovery_hold.proof` exposes the attributed decision and evidence references;
`null` means no clearing decision. Pass an evidence ID to `evidence_get` with an
explicit byte offset and limit (1–32768). Recovery materials are marked
`kind: recovery_operator_material` and include project, snapshot, decision and
provenance, with exact bytes and optional UTF-8 text. The reader verifies a unique
committed admission for this snapshot and the entire file hash before paging.
Missing, altered, unregistered or symlinked materials fail closed. A verified hash
establishes stored-byte integrity only; it does not authenticate an operator or
establish task acceptance. These reads do not resume work or change recovery state.


### Discarding reconciled recovered executions

The ledger has a distinct `RecoveryExecutionDisposition` maintenance operation for
explicitly abandoning unknown recovered executions. It requires a review with
admitted material references, a bounded exact step list, and both
`processes_confirmed_stopped` and `external_effects_reconciled` set by a trusted
operator after independent checks. This operation does not itself inspect remote
systems, stop processes, authenticate the operator or prove those assertions.

It records `recovery_executions_discarded` and marks the listed unknown executions
cancelled atomically. It never claims task success, refunds attempts or retries.
Project holds and global activation gates remain; unknown verification ownership
needs its own reconciliation. Use the explicit offline command after completing
those external checks:

```sh
target/debug/telos backup discard-executions \
  --image /absolute/recovery/prepared-state \
  --expected-receipt RECEIPT_SHA256 \
  --decision /absolute/disposition.json \
  --materials /absolute/operator-materials \
  --destination /absolute/disposed-image
```

The decision file wraps a project review (with the same report filename/hash
format documented above) and names the exact execution records:

```json
{
  "review": {
    "project_id": "PROJECT_ID",
    "manifest_sha256": "BACKUP_MANIFEST_SHA256",
    "decision_id": "execution-disposition-001",
    "operator": "OPERATOR_ATTRIBUTION",
    "statement": "DESCRIPTION_OF_ACTUAL_PROCESS_AND_EFFECT_CHECKS",
    "reviewed_external_state": true,
    "evidence": [{"id": "report.txt", "sha256": "REPORT_BYTES_SHA256"}]
  },
  "step_ids": ["EXACT_UNKNOWN_STEP_ID"],
  "processes_confirmed_stopped": true,
  "external_effects_reconciled": true
}
```

Do not set the confirmations merely because Telos stopped or the snapshot is old.
They attest to the operator's independent checks; the command cannot establish
those facts. Material bytes, project/snapshot attribution and named task states
are checked before disposition. No MCP tool exposes this authority.

The command uses the same new-copy publication workflow as `review-project`:
source bytes stay unchanged, destination must be new, failures retain gated
candidates, and output supplies a receipt hash for chaining. The receipt carries
the disposition and is cross-checked against the audit and cancelled step states.
The new image still has held projects and paused goals. A later `review-project`
can consume it with a distinct review decision after all other uncertainty is
resolved; the disposition proof remains in its receipt and audit history.
This does not activate the service or retry tasks.


### Discarding independently reconciled unknown verification

For a restored unknown verification lease, use `backup discard-verifications`
with the same `--image`, `--expected-receipt`, `--decision`, `--materials` and
`--destination` arguments as `discard-executions`. Its decision envelope contains
`review`, `lease_ids` (1–100 exact IDs), `processes_confirmed_stopped: true` and
`external_effects_reconciled: true`. It does **not** accept `step_ids`.

Use a distinct attributed decision and supply material from actual independent
checks of the verifier processes and external effects. Original-path cleanup
reports are not automatically portable between restored directories or hosts.
This command does not inspect remote systems, terminate processes or establish
operator identity. Never infer those confirmations from a missing process report.

The transaction changes only named `outcome_unknown` verification leases in the
reviewed project to `finished`, records a discarded observation, and preserves all
verification attempts and prior test evidence. Finished here means ownership was
resolved, not that the tests passed. Unknown executions, project holds, paused goals
and global activation gates remain. No tests or retries launch. Ordinary MCP cannot
perform this operation.

The output is a new gated image with a `verification_disposition` receipt entry.
Its proof must match a unique committed audit and the listed finished lease states.
It can be consumed by later offline disposition/project-review operations; existing
source images and destinations are never overwritten. Independent real-world
checks and subsequent activation remain separate from receipt integrity.


### Inspecting and adopting a recovered worktree

The recovery runtime can reconnect an independently registered Git worktree using
`RecoveryWorkspaceAdoption`: a material-bound `review`, `workspace_id`, absolute
canonical `candidate_path`, `expected_snapshot_sha256` and an explicit
`exclusive_access_confirmed` attestation. It checks reciprocal Git identity and two
consistent snapshots of Git-visible files plus configured acceptance paths. This
is an operator-reviewed current fingerprint, not a claim that all bytes equal an
old backup or that ignored dependencies are captured.

Adoption requires resolved project execution/verifier uncertainty, the current
policy revision, and a paused delivery goal with a blocked cycle. It updates only
the binding and readiness, advances an existing verification plan version to make
old acceptance stale, and records the previous binding/observed snapshot atomically.
It never edits Git metadata, moves files, runs tasks, resets attempts or activates
restored state. Candidate files must remain exclusively controlled through later
activation; the attestation does not enforce an OS-level external-writer lock.

First obtain a pinned observation from the verified prepared image:

```sh
target/debug/telos backup inspect-workspace \
  --image /absolute/recovery/prepared-state \
  --expected-receipt RECEIPT_SHA256 \
  --workspace-id WORKSPACE_ID \
  --candidate /canonical/registered/worktree
```

This verifies the source receipt before inspecting the explicitly selected external
path. It returns `snapshot`, `snapshot_sha256`, project/workspace identity and the
source receipt/snapshot identities. The source database is opened read-only and
unchanged; no Git repair, file move, worker or verifier starts. Stop a review daemon
on that image before maintenance so the image ownership lock can be acquired.

Prepare an adoption decision with the usual material-bound `review` plus:

```json
{
  "review": {"...": "FULL_PROJECT_REVIEW_OBJECT_AS_DOCUMENTED_ABOVE"},
  "workspace_id": "WORKSPACE_ID",
  "candidate_path": "/canonical/registered/worktree",
  "expected_snapshot_sha256": "SNAPSHOT_SHA256_FROM_INSPECTION",
  "exclusive_access_confirmed": true
}
```

The abbreviated `review` above must be replaced with the complete typed review,
including material file IDs/hashes, attribution and snapshot identity. Confirm
exclusive access only after actual independent checks. Submit it using:

```sh
target/debug/telos backup adopt-workspace \
  --image /absolute/recovery/prepared-state \
  --expected-receipt RECEIPT_SHA256 \
  --decision /absolute/adoption.json \
  --materials /absolute/operator-materials \
  --destination /absolute/adopted-image
```

The adoption reobserves the candidate and requires the pinned fingerprint. Only a
new recovery copy is changed; failure preserves the source. The published receipt
binds the adoption to its audit, current worktree identity/path/readiness and new
verification-plan version. Later project review can consume this output without
losing the adoption proof. File integrity does not imply current external ownership
or tests passing. The global recovery gate remains until the explicit activation workflow below
publishes a bound startup proof.


### Inspect activation blockers

```sh
target/debug/telos backup inspect-activation \
  --image /absolute/reviewed-image \
  --expected-receipt RECEIPT_SHA256 \
  --check-workspaces
```

The command verifies the pinned receipt and opens the ledger read-only. It reports
held projects, unfinished executions/verifiers, unpaused goals, armed schedules,
pending notifications and current (not historical closed-cycle) workspaces. At
most 100 workspace records are inspected; truncation is an explicit blocker.

`--check-workspaces` explicitly authorizes reading the adopted external worktree
paths in the verified image. Without it, such paths are not inspected and each
current adopted workspace remains unchecked. With it, reciprocal Git identity and
current file fingerprints must still match the adoption. Missing or changed
worktrees appear as blockers; no Git repair or filesystem mutation is attempted.

Any historical `state/runs` namespace is also a blocker. Normal startup scans that
namespace for process cleanup; activation must preserve old records separately so
snapshot process identities never authorize cleanup of current processes. This
check does not parse journals, signal processes, delete evidence or archive files.

`checks_passed` describes only these point-in-time checks. It is not activation
permission or proof of exclusive access, stopped source services, or external
process cleanup. `activation_supported` is true in the current source build, but
`execution_enabled` remains false for preflight. Use the explicit activation command
below; never remove restore markers or edit preparation records manually.


### Archive historical run records before activation

After resolving all unknown executions and verification leases:

```sh
target/debug/telos backup archive-runs \
  --image /absolute/reconciled-image \
  --expected-receipt RECEIPT_SHA256 \
  --destination /absolute/archived-image
```

The command copies the verified image and relocates `state/runs` to
`state/recovery-history/original-runs` inside the private candidate. It never parses
process journals or signals any process. The new ledger records the original
snapshot identity, fixed old/new namespaces, relative file-inventory hash and count.
File bytes and executable flags must match, and unrelated files must remain
unchanged. Existing archive destinations and unresolved work are refused.

Only a verified, audited candidate is published. If copying, movement, audit or
verification fails, the original stays intact and an unpublished gated candidate
may remain for diagnosis. Filesystem movement and SQLite audit are not a single
transaction; new-copy publication is what prevents exposing incomplete work as a
successful result. Retry from the verified original, never an incomplete candidate.
No files are deleted. Historical report contents and absolute path strings remain
as recorded; the archive is evidence, not a live process namespace.

The new receipt verifies the archive against both its file inventory and committed
audit. `inspect-activation` on the output no longer reports the old-run namespace
blocker. Project/workspace/ownership checks and formal activation still apply;
this command never removes recovery gates, resumes goals or starts a service.
No extra operator decision file is needed to archive already reconciled evidence;
the explicit command and pinned source select the new-copy operation.


### Activation authorization boundary (internal)

Source schema 19 adds a separate activation authorization record; it does not
change the prepared-recovery state or start the service. A typed decision binds
operator attribution, statement, snapshot hash, source receipt hash and one new
normalized target-state directory. It explicitly requires independent confirmation
that the source instance is retired and access is exclusive. These assertions are
not automatically established or authenticated by the ledger.

The internal transaction requires a matching successful preflight, rechecks all
project holds, unknown work, goal/schedule/notification state and current workspace
readiness, and refuses a remaining historical runs namespace. Authorization and
audit commit together. Repeating the exact decision/observation is idempotent;
changing the destination or other inputs cannot overwrite it. Goals stay paused.

Authorization alone is insufficient for startup; the command below additionally
performs external checks and publishes a target-bound proof. Both startup gates
remain effective if only an authorization record exists. Read-only
review continues to accept schema 18 images without migrating them; source writes
migrate private candidates to schema 19. These recovery-image operations do not
change the installed service state.


### Publish and start the restored state

After dispositions, workspace adoption, project reviews and historical run archival,
create a decision containing `manifest_sha256`, `source_receipt_sha256`, the exact
canonical final `target_state`, `decision_id`, `operator`, `statement`,
`source_instance_retired: true` and `exclusive_access_confirmed: true`.
`target_state` must be the new destination's `state` subdirectory.

Those confirmations must reflect actual independent retirement of the source
instance (including any service that could restart it) and exclusive control of
its external workspaces. The command also refuses existing local source daemon or
lifecycle locks while they are held; absent/moved source directories cannot establish
retirement automatically. It does not stop or uninstall the old service for you.

```sh
target/debug/telos backup activate-restore \
  --image /absolute/reconciled-image \
  --expected-receipt RECEIPT_SHA256 \
  --decision /absolute/activation-decision.json \
  --destination /canonical/new-restored-instance
```

The new destination must not exist. The command copies the verified image, checks
all ledger blockers and external adopted worktrees again, atomically records the
bound authorization/audit, and writes a publication proof. It verifies and fsyncs
the candidate before exclusive publication. Source images and evidence remain
unchanged; incomplete candidates are retained and cannot start at their temporary
paths. Existing activated images cannot be edited with recovery mutation commands;
use the retained pre-activation image to revise a recovery decision.

The result reports `activated: true`, `service_started: false` and
`goals_resumed: false`. Start the published instance explicitly:

```sh
target/debug/telos --state-dir /canonical/new-restored-instance/state daemon
```

Both recovery provenance and publication markers remain. Startup requires the
publication hashes, authorization row, audit and canonical target path to agree
before process cleanup or workers start. A copied/moved instance is rejected;
authorization without publication is also rejected. Archived old journals stay
outside the live cleanup namespace. Ordinary later restarts use the same static
publication proof rather than expecting a mutable database to retain its initial
file hash. Existing goals remain paused and schedules disabled until explicitly
resumed under normal project authority.

The output receipt verifies the initial publication; once the service mutates its
ledger it is historical, not a live-state checksum. Re-backing-up and restoring an
already restored/activated lineage still requires a separate generation workflow;
that workflow and real operational restore pilots have not yet been accepted.
Live evidence retention and long-duration operational reliability remain
separate validation work.

## Reconcile an external environment session in a recovery copy

Use this after independently stopping the old session's local/remote operations
and reconciling the existing provider protocol. The tool records that review; it
does not perform or authenticate those external steps. Stop any recovery-review
daemon on the input image before maintenance. Pin the exact source `receipt.json`
SHA-256 and choose a destination that does not exist:

```sh
target/debug/telos backup reconcile-environment \
  --image /absolute/recovery/prepared-state \
  --expected-receipt RECEIPT_SHA256 \
  --decision /absolute/environment-decision.json \
  --materials /absolute/operator-materials \
  --destination /absolute/environment-reviewed-image
```

Example decision (replace placeholders with the actual review; timestamps are
integer Unix seconds and must be within five minutes when applying):

```json
{
  "review": {
    "project_id": "PROJECT_ID",
    "manifest_sha256": "BACKUP_MANIFEST_SHA256",
    "decision_id": "environment-review-001",
    "operator": "OPERATOR_ATTRIBUTION",
    "statement": "DESCRIPTION_OF_ACTUAL_PROVIDER_AND_PROCESS_REVIEW",
    "reviewed_external_state": true,
    "evidence": [{"id": "provider-review.txt", "sha256": "MATERIAL_SHA256"}]
  },
  "session": {
    "loop_id": "LOOP_ID",
    "revision": 1,
    "session_id": "SESSION_ID",
    "environment_sha256": "ENVIRONMENT_SHA256",
    "adapter_sha256": "ADAPTER_SHA256",
    "expected_sequence": 123,
    "expected_version": 2,
    "decision_id": "environment-review-001",
    "provider_observed_at": 0,
    "provider_lease_owner": null,
    "provider_operation_active": false,
    "remote_operations_stopped": true,
    "local_owners_retired": true,
    "provider_protocol_reconciled": true,
    "operator_note": "DESCRIPTION_OF_RECONCILED_SESSION"
  }
}
```

Place the independently reviewed material at `operator-materials/provider-review.txt`;
it must match the supplied SHA-256. The material may describe real provider status
and process checks, but must exclude private session tokens. Obtain the session's
exact original contract revision, journal event sequence/version and both hashes
from `loop_verification.external_environment` or paginated audit records. A provider
still reporting this session as owner is rejected. Another owner may legitimately
be active; retiring this session does not grant permission to acquire the resource.

The output contains its new receipt hash and leaves execution disabled. To review
another session, use that output image/hash as the next input and a new destination.
All environment dispositions remain in the chained receipt and audit; tampering,
missing entries or duplicate entries are rejected. Project holds remain held and
goals stay paused. Afterwards, use the separate project-review and activation
steps, resolving any other execution/verification/workspace blockers first. This
operation is not exposed as an ordinary MCP tool. The private operator path trusts
the OS user and is not containment against arbitrary same-user shell access.

Fixture tests cover two acquired journal sessions, invalid snapshot/project/material/
receipt/decision/version/freshness inputs, source preservation, retained holds,
material readability, chained project review, and forged/removed/duplicate receipt
entries. They do not establish a real external lease or production incident recovery.
