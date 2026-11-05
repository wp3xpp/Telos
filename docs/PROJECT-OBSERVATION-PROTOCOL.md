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

For a six-hour review plus five-minute recovery, configure separate criteria and
window policies (21600 and 300 seconds). The analysis interval, collection cadence,
probe polling and post-action observation window are distinct settings.

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
