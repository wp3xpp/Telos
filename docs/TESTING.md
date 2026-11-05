# Independent local test commands

`telos test-command` is an operator-only execution probe. It runs a configured
executable directly, without a model, inside a fresh Codex sandbox profile and
records actual exit status and bounded output. It does not yet configure a goal's
verification plan, create a durable ledger claim, or complete a Loop.

Example request (runtime read roots must match the installed toolchain):

```json
{
  "program": "/bin/sh",
  "args": ["./scripts/test.sh"],
  "readable_roots": [],
  "timeout_seconds": 60,
  "max_output_bytes": 1048576
}
```

```sh
telos test-command --request /absolute/test.json \
  --cwd /absolute/worktree --output /absolute/evidence/new-run \
  --codex /absolute/path/to/codex
```

The output directory must be new, its parent must exist, and it must be outside
the writable workspace. The executable and runtime read roots must be absolute.
Arguments are passed literally; selecting a shell explicitly authorizes that
shell to interpret its own arguments. This is not a tool for untrusted model
requests to grant arbitrary execution authority.

The runner uses an isolated HOME and CODEX_HOME, with no copied credentials or
user configuration. The environment is cleared except PATH, TMPDIR, LANG and
LC_ALL. The permission profile extends `:workspace`, denies the filesystem root,
allows `:minimal` and explicitly configured toolchain paths for reading, and
disables network access. See the official [permission profile documentation](https://learn.chatgpt.com/docs/permissions).
The runner invokes `codex sandbox --include-managed-config -P ... -C ... -- ...`.
Check CLI compatibility when upgrading Codex. Profile or managed-policy errors
cannot produce a passing result.

`request.json`, `codex-home/config.toml`, `processes.json`, `stdout.log`,
`stderr.log` and `report.json` are retained. The report records executable,
profile and stream SHA-256 digests. A clean zero exit passes this command only;
nonzero exits fail, and timeout/cancellation/output overflow/uncertain cleanup
produce an error. CLI exit is nonzero for any non-passing report. A wrapper
startup failure may be reported as a failed command, never as successful tests.

Process identities are polled and journaled before cancellation and cleanup.
This provides bounded handling of observed child processes, not proof of
containment for arbitrarily fast reparenting or malicious concurrent filesystem
changes. Hard process termination/crash recovery is not integrated into the
daemon for these standalone runs yet. Digests record observations, not immutable
toolchain or project snapshots.

## Verification

`tests/test_runner.rs` uses a synthetic wrapper to verify literal arguments,
exit status, timeout, cancellation, output limits, observed child cleanup and
rejecting evidence inside the writable workspace. It does not prove sandboxing.

`scripts/test_command_smoke.py` uses the real Codex native sandbox, a synthetic
outside control file and a temporary loopback TCP listener. It verifies workspace
writes, denied outside reads/writes, denied networking, nonzero exit propagation
and output hashes. It does not call a model or touch other projects/credentials.

```sh
cargo build --locked
python3 scripts/test_command_smoke.py --output /absolute/new/probe-directory
```

## Delivery test acceptance

Operator-configured verification plans now accept `test_checks`:

```json
{
  "test_checks": [{
    "criterion_id": "regression",
    "command": {
      "program": "/bin/sh",
      "args": ["./scripts/test.sh"],
      "readable_roots": [],
      "timeout_seconds": 30,
      "max_output_bytes": 65536
    }
  }]
}
```

Configure through the existing private `configure_verification` operator request.
Each contract criterion must have exactly one file, log or test check. Tests are
currently delivery-only and require a ready owned worktree under the current
workspace-write policy. A command's timeout/output limit must fit that policy.
At most 16 commands and one hour total configured timeout are accepted per plan.
This does not grant network access or configure a remote/shared environment lease.

`loop_verify` and post-worker verification run those tests under exclusive project
verification ownership. Schema v15 reserves a test attempt before launching commands.
Each round consumes one of the contract's `max_attempts` test rounds, counted across
plan edits/restarts within the cycle. This budget is separate from development-step
attempt counting. Commands run serially with the daemon's configured Codex binary.

Acceptance compares worktree snapshots before and after all tests; changes reject
acceptance even with zero exits. Reports record executable/profile/output hashes.
Nonempty stdout/stderr streams are copied into immutable ledger evidence, bound to
that artifact and local workspace, and linked from criterion results. Empty streams
are represented by the report's empty-content hash. Passing tests still require a
saved review patch and current contract, plan, policy and artifact identity.

Pause/stop and daemon shutdown signal cancellation and wait for runner cleanup.
Unconfirmed cleanup or an execution error without a trustworthy report retains an
unknown lease. Process journals use the existing flat runs directory so startup
can clean observed interrupted processes. No uncertain test is automatically rerun.

The control client permits up to 3700 seconds for explicit verification (other
requests retain their shorter timeout). External MCP hosts may impose their own
limits; a caller timeout is not proof that testing stopped. Inspect status/events
before requesting another round. The current interface waits for a result; a
separate asynchronous verification-job API remains a UX improvement.

This binds observations to the code snapshot and local workspace. It does not
freeze or fully fingerprint dependency/runtime directory contents, and cannot
exclude direct user filesystem edits or changes that occur and revert between
snapshots. Remote/shared environment identity and leases and a real feature pilot remain outstanding. Bounded local automatic
correction is documented in [REPAIR.md](REPAIR.md).

Reproduce actual native sandbox acceptance without a model call:

```sh
cargo build --locked
python3 scripts/test_acceptance_smoke.py --output /absolute/new/probe-directory
```

The harness demonstrates failed tests keeping a goal active, then edits a synthetic
fixture and verifies passing tests complete it. It preserves the original checkout,
checks raw output/review evidence, and cleanly stops the daemon. It is not autonomous
repair or the real project pilot.

## Durable verification ownership

Schema v14 adds a project-exclusive verification lease. Existing file and log
verification now acquire it before observation and release it after the observation
future returns, including normal errors. Acceptance requires the exact running
lease, cycle, contract revision and plan version. A second verifier cannot acquire
ownership, and direct acceptance cannot bypass an active or unknown lease.

While ownership is active, the ledger rejects new project steps, workspace
preparation/state changes, policy changes and verifier-plan changes. Contract edits
and resume also require idle verification. Acquiring ownership requires no queued,
running or uncertain project execution and no workspace preparation. Pause and stop
remain available; late acceptance is rejected by the existing active-goal checks.
This deliberately serializes observations within one project; it is not a lock on
remote infrastructure shared across different projects or on external user edits.

Daemon startup converts interrupted running leases to `outcome_unknown` with an
audit event and notification. Unknown leases retain exclusion and cannot be cleared
by the ordinary completion method. There is no expiry that authorizes rerunning an
uncertain test. Operator reconciliation can discard an uncertain observation when cleanup is
independently supported, as described below. Missing cleanup proof still blocks
recovery. Do not edit SQLite to release ownership.

The standalone test-command CLI remains separate from goal state. The integrated
verifier uses these leases, releases only after known cleanup, and binds the test
outputs to its artifact before acceptance. No MCP permission-granting surface was
added. Unknown leases without sufficient cleanup evidence remain blocked and must not be
bypassed by raw SQLite edits.


## Inspecting and reconciling uncertain verification

`loop_verification` is a read-only MCP tool taking a goal ID. It returns the active
or unknown verifier blocking that project, including the owning goal, cycle and
revisions. The owner may be a different goal in the same project. A null active
lease means no current verification lock; it does not prove goal completion.

Trusted operators can send the following through `telos request`:

```json
{"op":"reconcile_verification","lease_id":"verification-lease-..."}
```

This is deliberately not exposed as a model MCP tool. It does not accept a caller's
pass verdict or cleanup claim. The runtime loads the original configured commands
and independently reads the run directories. For every configured test it requires:

- A completed report belonging to that run with cleanup_confirmed=true.
- An exact request match and profile/output hashes matching the report.
- A bounded regular process journal with no recorded identity still alive.
- Regular bounded evidence files and unchanged, non-symlink parent directories.

Missing runs, partial reports, changed bytes or live recorded processes reject
recovery and preserve ownership. The recovery path never kills processes; startup
cleanup remains responsible for observed interrupted workers. Process identities
and reports provide evidence within the existing polling model, not proof against
arbitrary fast reparenting or malicious edits by the local state owner.

If the ledger shows no test-attempt reservation, no test was authorized to launch;
that read-only observation may be discarded without test-process evidence. After
successful inspection the ledger atomically finishes the unknown lease and emits
verification_reconciled with proof hashes, disposition=discarded and automatic_retry=false.
The goal state, code, acceptance history and consumed attempts are unchanged. A
subsequent test is an explicit new attempt under the remaining budget. A repeated
reconciliation request fails because the lease is no longer unknown; it has no
additional effect.

This covers interruption after completed cleanup but before ledger finalization,
and observations interrupted before test reservation. It cannot release crashes
with missing cleanup reports, incomplete multi-command runs, or external effects
that require their own environment reconciliation. Those cases remain blocked
pending stronger evidence or an environment-specific recovery adapter.


## Generic project observation/recovery

```sh
python3 scripts/observation_adapter_live.py --output /private/tmp/telos-observation-unique
```

Requires macOS and the installed Codex CLI. Uses a disposable project and synthetic
worker/action driver, actual native read-only observation probes and actual Codex
sandbox delivery verification. It tests failure, bounded collection, duplicate
physical episodes, missing/stale/incomplete coverage, stage association and fresh
independent post-action recovery. It does not contact or deploy production.
