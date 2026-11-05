# Project-owned observation adapters

Telos manages goals, execution ownership, bounded collection receipts, independent
verification and recovery gates. A project adapter owns its source transport,
log grammar, business-health predicates and action scripts. Adding a new project
must not require adding its log parser, deployment command or health thresholds
to the Loop engine.

Use this protocol for observation integrations. Project-owned adapters normalize
their source data into this schema; the framework does not bundle format-specific
parsers or source collectors.

## Separate collection from analysis

A six-hour analysis interval is not a six-hour collection interval. Run a
project-owned collector on its appropriate cadence (for example once a minute),
retain its evidence outside worker-editable worktrees, and use a continuous goal
for the separate analysis schedule. Telos does not install or start that external
collector merely because a goal was recorded.

A collector may use its already authorized transport, including a project SSH
wrapper. Telos's observation command is a read-only, network-disabled consumer of
local evidence. It receives no copied credentials. Transport permissions and
collector scheduling remain explicit project/operator configuration.

A project can use the operator socket to record normalized collection metadata:

```json
{
  "op": "record_observation_batch",
  "id": "CONTINUOUS_LOOP_ID",
  "revision": 1,
  "batch": {
    "source_id": "project/service",
    "request_key": "stable-range-request-key",
    "expected_version": 0,
    "generation": "source-generation-1",
    "start": 0,
    "end": 262144,
    "source_size": 1048576,
    "gap_reason": null,
    "signals": [
      {"resource_key":"physical-resource-A","episode_key":"recovery-1","kind":"unavailable"}
    ],
    "evidence": ["sha256:PROJECT_CAPTURE_DIGEST"]
  }
}
```

Send typed requests through `telos --state-dir /absolute/state request FILE.json`.
These are operator operations, not new agent MCP tools or permission grants.
`observation_checkpoint` with `id`, `revision`, `source_id` returns version/cursor.

Each batch covers at most 256 KiB. A cursor advances atomically with its audit
receipt. Reusing the same request key/content returns the existing receipt;
conflicting content or a stale cursor version is rejected. Continue from the
last accepted offset. Rotation, truncation, skipped history or another offset
gap requires an explicit `gap_reason`; it must not be reported as complete coverage.

Drain additional ranges within a finite per-tick budget instead of silently
jumping to the tail. `telos_core::observation::CatchUpBudget::next_limit` returns
the next permitted range size, capped by remaining batch count, total byte budget,
backlog and 256 KiB. Stop when it returns zero and retain remaining backlog for
the next collector tick. The project collector must implement this loop; recording
one batch does not magically fetch subsequent bytes.

Signals are deduplicated across batches by source, contract revision,
`resource_key`, `episode_key`, and `kind`. Project adapters map logical fanout to
the actual physical resource and recovery episode. The framework does not infer
those business relationships from text. Collection receipts and
`loop_context.project_observations` always say `business_health: unverified`.

## Independent project checks

An operator configures `local.observation_json` in `VerificationPlan.test_checks`:

```json
{
  "criterion_id": "service-health",
  "adapter": "local.observation_json",
  "observation": {
    "source_id": "project/service",
    "window_seconds": 300,
    "max_age_seconds": 120,
    "poll_interval_seconds": 30,
    "after_stage": "activate"
  },
  "command": {
    "program": "/absolute/project-adapter/health-probe",
    "args": [],
    "readable_roots": ["/absolute/project-evidence"],
    "timeout_seconds": 30,
    "max_output_bytes": 131072
  }
}
```

The executable, its runtime read roots and evidence inputs are trusted operator
configuration. Keep them outside a delivery worker's editable scope. The current
project policy must be enabled and its timeout/output bounds must cover the probe.
Continuous workers and diagnoses remain read-only even when the project also has
workspace-write authority for separately scoped delivery goals.

On macOS the observation command uses a framework-generated native read-only
sandbox: file writes and network access are denied, and file content reads are
limited to declared inputs, the project, isolated home and required system/runtime
paths. Ancestor directories can be listed for runtime startup. This is separate
from the Codex workspace-write sandbox used for delivery tests; inheriting a
workspace profile does not establish a truly read-only observation environment.
The generated `readonly.sb` and its hash are retained with the process report.

The runtime provides `TELOS_OBSERVATION_CONTEXT` as one JSON environment value:

- `loop_id`, `revision`, `cycle_id`, `criterion_id`, `source_id`;
- unique `request_id` for this verifier lease;
- requested `window_start` and `window_end` (Unix seconds);
- optional `not_before` and `subject_id` for a post-action recovery check.

The adapter reads its evidence, checks actual project predicates and emits exactly
one JSON object to stdout. Echo the binding without modification:

```json
{
  "binding": {
    "loop_id": "LOOP_ID", "revision": 1, "cycle_id": "CYCLE_ID",
    "request_id": "CURRENT_LEASE_ID", "criterion_id": "service-health",
    "source_id": "project/service", "window_start": 100, "window_end": 400,
    "not_before": null, "subject_id": null
  },
  "observed_at": 400,
  "coverage": {"start":100,"end":400,"complete":true,"gap":false,"backlog_bytes":0},
  "verdict": "passed",
  "summary": "Project predicates measured against current evidence",
  "evidence": ["sha256:ACTUAL_MEASUREMENT_DIGEST"],
  "signals": []
}
```

This is a schema example, not a reusable passing result. The runtime supplies real
bindings/times. The probe must verify the requested subject against measured data,
not merely echo a desired subject and assume it is running. `verdict` is one of
`passed`, `failed`, `inconclusive`, or `error`; stderr can contain bounded diagnostics.

The framework rejects stale/wrong source, cycle, revision or request bindings;
limits output; checks freshness and complete requested coverage; and refuses a
pass with gaps, backlog, or no measurement references. A registered collection
backlog or audited gap in this window also overrides a claimed pass. A zero process
exit is not a health result. Empty log events do not establish freshness or health;
an actually healthy quiet period needs complete coverage and real measurements.

Project predicates decide what the measurements mean. They can check data ages,
RPC corroboration, process identity or any other project-specific facts. References
and hashes establish provenance, not the factual truth of arbitrary adapter code.

Continuous probe polling is separate from development attempt budgets: polls are
read-only and paced by `poll_interval_seconds`; each has bounded time/output and
exclusive durable verification ownership. Automatic polling only occurs for an
explicitly armed current continuous schedule. Creating/configuring a goal alone
does not start recurring work. Pause/revision/policy changes and unknown ownership
retain their existing fencing behavior.

## From diagnosis to a controlled action and recovery

1. A failed independent check may produce one read-only diagnosis within the goal's
   step budget. Missing data remains inconclusive.
2. Create a separately scoped delivery and associate it with that diagnosis using
   the existing remediation relationship. Its workspace and tests are independent.
3. A project-owned action driver performs its already authorized test/release
   procedure. The generic engine does not embed a business deployment script.
4. An operator may submit `record_project_stage` on the accepted delivery:

```json
{
  "op": "record_project_stage",
  "id": "ACCEPTED_DELIVERY_ID", "revision": 1,
  "receipt": {
    "request_key": "stable-action-receipt-key",
    "stage": "activate",
    "artifact": "EXACT_ACCEPTED_DELIVERY_ARTIFACT",
    "subject_id": "ACTUALLY_OBSERVED_TARGET_FINGERPRINT",
    "outcome": "succeeded",
    "evidence_refs": ["sha256:PROJECT_DRIVER_OUTPUT_DIGEST"]
  }
}
```

`stage` names are project configuration. The receipt is an operator attestation,
not an action launcher, new authorization, independent release proof or health
pass. It requires a completed delivery and its exact accepted artifact. Conflicting
reuse of a request key is rejected. `failed` and `outcome_unknown` do not open the
recovery gate; unknown external effects require separate reconciliation, not retry.

5. A check with `after_stage` uses the current associated delivery's matching
   receipt. Until that receipt is present, acceptance remains inconclusive.
   Thereafter, the binding includes its subject and the ledger recording time.
   The entire `window_seconds` of coverage must begin at or after that time.
   A 300-second policy therefore requires a fresh five-minute window; old healthy
   history cannot satisfy it. The project probe independently checks the actual
   measured subject. Child completion alone never establishes parent recovery.
6. Only after the parent's independent recovery result passes may a project-owned
   coordinator perform a separately authorized finalization step, such as its
   commit/push procedure. This API does not automatically run that procedure.

The contract's analysis interval, the collector cadence, probe polling, and the
required coverage window are separate settings. For example, a multi-hour review
can use minute-by-minute collection and a five-minute post-action coverage window.

## Check readiness for an existing goal

Use the existing Loop ID and current contract revision throughout onboarding.
Track these independently, since collection can run before managed analysis starts.

```sh
telos --state-dir /absolute/state readiness --id LOOP_ID
telos --state-dir /absolute/state readiness --id LOOP_ID --json
```

The command reads one ledger snapshot from the running daemon. It reports missing
or disabled execution policy, missing verification plan, receipts for the plan's
sources, and the inspection schedule. An unarmed inspection displays `not scheduled`
with a null `inspection.next_due_at`. An armed inspection with an open cycle also
has a null next due time until that cycle closes. `first_verification` is the first
recorded verdict for the current contract and plan versions, or null if none exists.

| State | Read-only evidence available today |
| --- | --- |
| Goal recorded | `loop_get` returns the registered project, goal kind and current revision. |
| Collection enabled | Inspect the project collector's service status and a recent capture. Read `observation_checkpoint` or `loop_context.project_observations` for the matching source's cursor, backlog and gap. A checkpoint alone can be historical. |
| Inspection armed | `project_inspect` reports the current enabled execution policy. `loop_schedule.armed` must be true for the current contract and policy revisions. When false, display “not scheduled” even if `next_due_at` has a value. |
| First verification passed | Check `loop_context.latest_verification` and its evidence for a current-revision pass using the intended source and observation window. Report the measured time separately from the schedule. |

Before arming, confirm `telos_status` advertises `local.observation_json` and
check the current `verification_plan_configured` audit event with `loop_events`
and `event_body`. Compare the plan version, criteria, probe inputs and source IDs
with the intended configuration. `loop_verification` reports ownership blockers.
An empty observation list or absent verdict should be reported as missing evidence.

Check collector liveness and input schema version using project-supplied evidence.
The project collector also records collection failures in its evidence so the
probe can account for them.

## Minimal project integration and first acceptance

1. **Collect.** Run a project-owned service with its own read-only source access.
   At each tick, resume the committed generation and offset. Read bounded ranges
   until caught up or the tick budget is spent. Size the cadence and budget from
   measured input growth, including bursts. Record the source, evidence schema
   version, measured subject fingerprint, collection window, cursor, backlog and
   gaps in local evidence. Record failed attempts separately and leave the cursor
   in place. Persist evidence before submitting `record_observation_batch`. Keep
   the same request key and payload when retrying an uncertain submission.
2. **Measure.** Use a local probe that reads the collector's evidence, validates
   the schema and evaluates project predicates for the requested window. Include
   an actual freshness measurement and deduplicate by physical resource and
   recovery episode. Return inconclusive for missing, stale or incomplete evidence,
   unresolved rotation gaps, backlog or unavailable collection. A quiet window can
   pass when independent measurements establish full coverage and fresh data.
3. **Compare before arming.** Collect a real read-only window with approved source
   access. Run the project predicate in shadow mode against that evidence and
   compare the outcome, source coverage and episode counts with a manual review of
   the same window. Exercise the negative cases above using copied evidence. Run
   the isolated protocol harness to check Telos binding and verdict handling.
   `loop_verify` records a verdict and requires an active cycle, so use the project
   probe or an isolated test cycle for shadow evaluation.
4. **Enable and check the first run.** Once authorized, configure the existing
   goal's project policy and acceptance plan through `configure_project` and
   `configure_verification`, then start that goal with `loop_start`. Check the
   accepted step, armed schedule, recorded observations and first independent
   verdict. Keep “inspection armed” separate from “first verification passed.”

Track detection, repair delivery and operational recovery separately. When a
verified failure needs a fix, associate its diagnosis with a bounded delivery.
Run any approved operational action through the project's own driver. Use the
measured subject fingerprint and the fresh post-action window described above to
verify recovery before any separately authorized finalization. Reconcile an
unknown action outcome before issuing another action.

## Recovery design for interrupted inspection

The operator ingestion route accepts current-revision continuous Loops in
`active` or `blocked`. A collector can keep submitting evidence while an
operator reviews an interrupted inspection. Each receipt advances the cursor
atomically. If submission is interrupted, retain the pending bytes and replay
the exact request key and payload. Acknowledge the batch locally after Telos
returns the committed receipt. Inspect any version conflict before proceeding.

For a managed read-only inspection from an older runner that exited cleanly
but was marked unknown after output truncation, an operator can submit
`discard_inspection` through the local request interface. Bind the decision to
the current blocked Loop and its execution evidence. Review the external
effects independently before attesting reconciliation.

```json
{
  "op": "discard_inspection",
  "review": {
    "loop_id": "LOOP_ID",
    "revision": 1,
    "expected_version": 6,
    "cycle_id": "CYCLE_ID",
    "step_id": "STEP_ID",
    "generation": 1,
    "evidence_id": "EXECUTION_EVIDENCE_ID",
    "evidence_sha256": "EXECUTION_EVIDENCE_SHA256",
    "decision_id": "operator-decision-key",
    "external_effects_reconciled": true
  }
}
```

The daemon checks the recorded process identities, verifies the evidence hash
and matches the recorded outcome and run generation to the ledger. The receipt
records the disposition and preserves the original execution evidence and
attempt count. Replaying the same decision returns the same receipt. The goal
remains blocked until an operator resumes it. After collection catches up,
resume the original cycle under existing authority and verify a fresh, fully
covered window. Existing recovery and external-ownership fences still apply.

For new read-only workers, diagnostic bytes are retained up to the configured
budget, and complete events are parsed as they arrive within a separate frame
bound. Truncation is recorded in the report. Completion requires a valid
terminal event, a bounded structured result, a clean exit and confirmed process
cleanup. Write workers and independent test commands retain their output limits
as execution bounds.

## Reproduce protocol acceptance

```sh
cargo build --locked
python3 scripts/observation_adapter_live.py --output /absolute/new/observation-demo
```

The harness uses a synthetic read-only inspection/diagnosis worker and fixture
actuator, actual macOS native read-only observation probes, and actual Codex sandbox
delivery tests. It exercises failed assertions,
stale/empty/wrong-source/gap/backlog observations, read-only write rejection,
physical-episode deduplication, cursor catch-up, delivery association, action
attestation and a fresh independently measured recovery window. It is not a
production deployment or a real business adapter.
