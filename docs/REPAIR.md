# Automatic delivery correction

A user starts a bounded delivery through `loop_start` under an enabled project
workspace-write policy and an operator-prepared worktree. After a succeeded worker,
independent verification runs. A current failed verification can enqueue one
correction step automatically; execution success alone never closes the goal.

## Eligibility and ownership

The ledger discovers and transactionally rechecks the same predicate. Correction
requires an active current-contract delivery cycle, current policy and verifier
plan, ready matching workspace, and an artifact-bound failed verification. At least
one managed write step under this authority must already have succeeded. Merely
creating or verifying a goal does not arm autonomous writing.

The project must have no pending/running/unknown execution or verification lease.
The snapshot's recorded step count must still match the cycle. Both the project
step budget and contract max_attempts must have capacity. Test plans also require
remaining test-round capacity, so a repair is not launched without budget to verify
it. Paused/stopped/completed goals, stale plans, inconclusive/error verification and
uncertain execution are ineligible.

The enqueue key is tied to the failed verification ID. Concurrent schedulers or
restart cannot enqueue it twice. The queue, binding event and key are committed
atomically. A subsequent failure may create a new correction only within the same
budgets. If a worker fails or reports a blocker, that failure does not blindly
relaunch the same correction.

## Task and artifact binding

Before enqueue, the runtime compares the live worktree snapshot against the failed
artifact. It builds a bounded task containing the contract, workspace, failure
identity, original project memory, prior outcomes and up to six verified evidence
excerpts. Excerpts may be truncated; the prompt marks them as untrusted data rather
than permissions. It asks for an implementation correction, preserving prior work,
without weakening acceptance, editing the original checkout, Git history or
publishing/deploying.

Before worker dispatch, the runtime rechecks the verifier-plan version and live
snapshot again. These ledger reads occur before the execution/heartbeat select to
avoid holding the single SQLite connection inside a suspended execution future.
The filesystem checks do not provide atomic exclusion against external user edits;
they detect ordinary changes at the recorded boundaries.

A preparation failure records one `repair_deferred` event per failed verification
and suppresses repeated preparation attempts for it. Fresh explicit verification
can produce a new candidate after the underlying condition is addressed. No
background retry storm or automatic authority expansion occurs.

## Completion and limits

After a successful correction worker, existing independent acceptance runs again.
Only passing current artifact-bound tests/checks with saved review evidence can
complete the delivery. A failed independent test is eligible for further correction;
an error/inconclusive outcome is not. A nonzero test exit alone cannot establish
whether the defect is application code or environment, so the agent is instructed
to report environmental blockers instead of inventing a code fix. Iteration remains
bounded even when diagnosis is wrong.

This is local delivery correction. Continuous monitoring remains read-only diagnosis;
remote/shared-environment remediation needs its own authority and lease adapter.
Dependency/runtime contents are not fully frozen, and arbitrary desktop control,
merge/publish and production deployment are not part of this adapter.

## Validation

Synthetic worker integration covers failed tests -> correction -> passing tests,
repeated failure stopping at budget, no writing before managed start, pause, changed
plans/artifacts, duplicate enqueue and pre-dispatch plan validation.

The real integration harness uses a synthetic initial worker to force a deterministic
failure, then delegates the correction to the authenticated Codex CLI and runs actual
native-sandbox acceptance. It preserves the source checkout, checks stored test and
review evidence, and stops the daemon cleanly:

```sh
cargo build --locked
python3 scripts/repair_smoke.py --output /absolute/new/repair-evidence
```

This consumes model quota. It is an isolated integration probe, not the real feature
pilot. The saved report identifies the synthetic initial attempt explicitly.
