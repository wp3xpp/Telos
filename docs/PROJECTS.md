# Project execution configuration

The current operator interface accepts typed JSON through `telos --state-dir
/absolute/state request request.json`. The daemon must already be running against
that same state directory. Do not edit SQLite or grant authority from model tools.

Register a project with an absolute directory:

```json
{"op":"register_project","id":"example","root":"/absolute/project"}
```

Registration makes the project discoverable through `project_list` and
`project_inspect`; it does not enable managed execution. Configure existing user
authorization separately, through this operator request:

```json
{
  "op": "configure_project",
  "id": "example",
  "expected_revision": 0,
  "policy": {
    "enabled": true,
    "sandbox": "workspace_write",
    "timeout_seconds": 300,
    "max_output_bytes": 1048576,
    "max_steps_per_cycle": 2
  }
}
```

Revision 0 means no prior policy. Subsequent changes require the current policy
revision, available in `project_inspect.execution_policy`. The daemon audits changes
and cancels queued managed steps bound to the previous policy. Running or uncertain
managed work prevents policy replacement; pause and reconcile it first. Reconciliation
UI is still pending, so this release is not ready for unattended production use.

For an ordinary local delivery, the frontend uses the installed Skill and
`deliver --file PACKAGE.json --wait`. This operator shortcut checks existing
workspace-write authority, creates the delivery goal, prepares an owned Git
worktree, configures independent acceptance and starts background execution.
Users state requirements and acceptance criteria in chat; the frontend prepares
the package. It never creates permissions merely because a goal exists.

The individual operations remain available: create a delivery Loop, prepare its
worktree for the current contract/policy revision and committed base, configure
verification, then call `loop_start` with the current revision and a stable
request key. Retrying an identical start key returns the same step. Use a new key
only for a genuinely new authorized step. Managed continuous goals use read-only
inspection; managed write execution is limited to delivery goals in prepared,
matching worktrees. See [DELIVERY.md](DELIVERY.md).

The legacy `enqueue` operator request is an explicit trusted execution path for
integration probes. It is not exposed to MCP and does not use the managed policy
grant. The private socket is not a security boundary against other programs
running as the same OS user.

Policy limits are scheduling checks; process/file/network containment depends on the
configured runner sandbox and project adapters. A readonly Codex sandbox does
not itself prove that every possible external tool has no side effects. No remote
deployment or arbitrary SSH adapter is configured by this interface.

## Independent acceptance

Choose checks that measure the contract's actual acceptance criteria:

- File hashes verify exact expected bytes; they do not prove functional behavior.
- `local.sandbox_command` runs independent local tests and checks their exit status.
- `local.sandbox_json` additionally reads one JSON assertion containing
  `criterion_id`, `verdict` and `summary`. A zero exit with `failed` does not pass.
- Normalized heartbeat/error [log-window checks](OBSERVATIONS.md) measure configured
  observations; source acquisition is not general business-health evidence.

Test checks require a prepared delivery worktree. Keep independent acceptance
scripts outside the worker-editable scope. Program paths, toolchain read roots,
timeouts and output bounds are operator configuration; see [TESTING.md](TESTING.md).

Configure via the trusted operator request, using a real lowercase SHA256 value:

```json
{
  "op": "configure_verification",
  "id": "loop-id",
  "revision": 1,
  "expected_version": 0,
  "plan": {
    "checks": [{
      "criterion_id": "artifact-content",
      "relative_path": "output.txt",
      "expected_sha256": "REPLACE_WITH_ACTUAL_64_CHARACTER_LOWERCASE_SHA256"
    }]
  }
}
```

Every contract criterion must have one check. Plans bind to the contract revision
and have their own optimistic version. The model-facing `loop_verify` tool only
executes configured checks; it cannot provide expected values or claim success.
Successful execution automatically invokes configured checks. If none is configured,
execution leaves the Loop awaiting verification.

The service independently reads bounded regular files inside the registered project,
records observed hashes and sizes as evidence, and verifies plan/contract versions
again before committing acceptance. A missing artifact is inconclusive, a differing
hash fails, and a failed read or invalid file is an error. Delivery goals complete
only on a full pass; continuous goals schedule their next due time. Explicitly
starting a managed continuous goal arms recurring dispatch. `loop_schedule` reports
the schedule state. Pausing suppresses dispatch; resuming allows one overdue cycle,
without replaying every missed interval. Contract or policy changes disable the old
schedule and require a new explicit start. Incomplete/failed cycles remain open;
bounded read-only failure diagnosis is available for eligible continuous cycles.
Delivery goals may run bounded automatic corrections after an independent failure
under existing write authority; see [REPAIR.md](REPAIR.md). Production remediation
requires separate acceptance and permissions. Installed binaries must support the
selected verifier; `telos_status` advertises the built-in adapter IDs.

## Supplying missing information

`loop_answer` records a user clarification against the current contract revision and
optimistic goal version. Reusing the same answer request key with the same content
is idempotent; using it with different content fails. Each answer is at most 8KiB,
with a 16KiB cumulative budget per contract revision. Consolidate requirements through
a contract revision if that budget is exhausted.

`loop_context` returns current-revision answers, bounded summaries and outstanding
work from the three most recent executions, plus the latest verification. New managed
steps include this context. Historical summaries are not new authority or acceptance
evidence. Answers remain in the ledger after restart; a revised contract does not
silently inherit old clarifications.

Answering does not resume work. If continuation is authorized, use `loop_resume` and
a new `loop_start` request key. Retrying the original start returns the original step
and its current state rather than replaying the work with changed context. An
`outcome_unknown` step cannot be cleared merely by supplying an answer or resuming.
