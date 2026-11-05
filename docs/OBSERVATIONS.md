# Normalized log-window observation

This adapter accepts a local, append-oriented JSONL observation source. It does not
parse arbitrary application logs. A project-specific normalizer must establish
the meaning of heartbeat/error records and preserve gaps rather than manufacture
heartbeats when the upstream source is unavailable.

Each newline-terminated record contains a nonnegative Unix timestamp in seconds,
`kind` (`heartbeat` or `error`) and an optional string `message`:

```jsonl
{"timestamp":1793692800,"kind":"heartbeat"}
{"timestamp":1793692801,"kind":"error","message":"upstream stream interrupted"}
```

Timestamps must not be in the future relative to the observing host. Initial
historical records are not used to prove a newly started healthy window. Source
messages are not copied into notifications. A heartbeat should attest the relevant
monitored condition; mere collector process liveness is insufficient for a service
health criterion.

An operator can configure a log criterion using the existing configure_verification
request. Hash checks and log checks may coexist, with exactly one check per criterion:

```json
{
  "op":"configure_verification",
  "id":"loop-id",
  "revision":1,
  "expected_version":0,
  "plan":{
    "checks":[],
    "log_checks":[{
      "criterion_id":"stream-health",
      "relative_path":"observations/stream.jsonl",
      "window_seconds":1800,
      "max_gap_seconds":30
    }]
  }
}
```

The service polls pending windows approximately every five seconds independently of
Codex execution. Choose a heartbeat gap with allowance for actual collection and
polling jitter. The minimum configured gap is five seconds; the maximum observation
window is one day. A source pause exceeding the gap restarts the window. Errors fail
the current observation and require a new full error-free interval before passing.
An error followed by a heartbeat in the same batch still reports the error first.
Expected warmup remains quiet; missing data, errors and recovery can notify.

Each sample reads at most 64KiB, with a 16KiB line limit. Trailing incomplete records
remain unread until complete. Backlog prevents acceptance. The persisted cursor
tracks device/inode, byte offset, a boundary-content anchor, timestamps and the latest
256 record fingerprints. Cursor compare-and-swap prevents concurrent observations
from consuming the same state twice. Observations and cursor updates share an audit
transaction; final acceptance additionally checks plan/contract versions.

Rotation, detected truncation/rewrite, invalid records, clock reversal, missing files
or excessive polling gaps cannot prove health. Device/inode plus a boundary anchor
detect ordinary log rotation and copy-truncate behavior; this is not a guarantee
against arbitrary rewriting elsewhere in an already consumed file. The adapter
expects an append-oriented source and does not provide an OS filesystem sandbox.

Reproduce the real-clock error/recovery test (synthetic executor and source):

```sh
cargo build --locked
python3 scripts/schedule_smoke.py --log-window --output /absolute/new/log-evidence
```

An explicitly started continuous goal can automatically investigate a `failed`
independent observation once per cycle. It must still have the same armed policy,
current verification plan, successful initial inspection, and spare budget under
both `max_attempts` and `max_steps_per_cycle`. Paused goals, missing evidence,
unknown execution outcomes, and ordinary healthy-window warmup do not trigger it.
The diagnosis receives bounded evidence excerpts, records its proposed correction,
and emits `diagnosis_ready`; it cannot write or mark the cycle recovered. A revised
plan or newer observation before dispatch cancels a stale queued diagnosis. Cancelling
or completing that diagnosis consumes the cycle's one automatic diagnosis slot.
Further investigation must be explicitly requested within remaining authority/budget.

Source collection over SSH, project-specific normalization, automatic writable
remediation, log retention, and an actual service pilot remain separate work. Audit history is now paginated: `loop_events` returns at most 20 entries, with
a fixed `through_sequence` watermark and `next_after_sequence`. Bodies larger than
8 KiB are explicitly omitted; `event_body` retrieves their exact JSON bytes in pages
of at most 32 KiB. This bounds transport size, but does not implement disk retention.

## Actual Telos control-interface source

The source build includes a foreground collector for a narrow, explicit criterion:
“the normal Telos control interface responds within the probe timeout.” It does
not measure worker progress, queue health, remote services or business correctness.

```sh
target/debug/telos --state-dir /absolute/live-telos-state observe-service \
  --output /absolute/monitor-project/observations/telos-control.jsonl \
  --timeout-seconds 3
```

The output parent must already exist. A successful normal Ping response emits a
heartbeat at the actual response time. Unreachable, timed-out, malformed or
recovery-review responses emit an error instead. Raw errors and response content
are not copied to the log. Exit success means the observation was recorded; inspect
`healthy` to distinguish a heartbeat from an error. A file/write failure returns an
error and cannot invent a heartbeat.

Use a dedicated output file for each state directory and criterion. Output uses
append with an exclusive advisory file lock, refuses symlink/nonregular outputs
and incomplete final lines, and syncs each newline-terminated record. The file
is bounded to 64 MiB; rotate explicitly before that limit. Both single-shot and
multi-sample collectors also hold a sidecar `<output>.collector.lock` for the whole
session. A second Telos collector for that output refuses to run. The lock file
remains after exit; the kernel lock, not file presence, establishes ownership. Do
not remove or replace that file while collection is running. Rotation interrupts the
healthy window as described above. Arbitrary external writers must be excluded.

By default the command records one observation. For a bounded foreground session:

```sh
target/debug/telos --state-dir /absolute/live-telos-state observe-service \
  --output /absolute/monitor-project/observations/telos-control.jsonl \
  --timeout-seconds 3 --samples 120 --interval-seconds 5
```

`--samples` accepts 1–1,000,000; the interval accepts 1–3,600 seconds. Each interval
starts **after** the previous probe/write completes. Delays never cause catch-up
probes or retrospective timestamps. Choose `max_gap_seconds` with allowance for
both probe time and scheduling jitter. Multi-sample runs return counts of recorded,
healthy and error samples plus the last observation; these counts do not themselves
prove an uninterrupted healthy window. Ctrl+C (SIGINT) or a service-manager stop (SIGTERM) cancels the current wait/probe and
reports `interrupted` without inventing a final heartbeat. Already appended records
remain; the next observer detects any subsequent gap.

This command does not install a launchd collector or an external scheduler. The
foreground process must stay running for the requested collection session; after
it exits there is no heartbeat. If Telos itself
is down, it cannot process or notify about its own failure until it returns; timely
out-of-process alerting requires an independent supervisor. A one-shot successful
probe is not a completed monitoring window or a long-duration pilot.

SSH collection and project-specific business-service normalizers remain separate.

Reproduce collector process ownership and signal handling without a live source:

```sh
cargo build --locked
python3 scripts/collector_lifecycle_smoke.py --output /absolute/new/collector-check
```

This check deliberately uses an offline state, so every observation is an error.
It verifies duplicate refusal, graceful SIGINT/SIGTERM exits, preservation of logs
and lock release. It does not install or claim launchd scheduling.

## Independent macOS collector service

The source build also manages a dedicated collector LaunchAgent:

```sh
target/debug/telos --state-dir /absolute/live-telos-state collector install \
  --directory /absolute/dedicated-collector --interval-seconds 5 --timeout-seconds 3
target/debug/telos collector status --directory /absolute/dedicated-collector
target/debug/telos collector remove --directory /absolute/dedicated-collector
```

Use an empty dedicated directory with an existing parent, separate from the monitored
state in both directions. For persistent macOS use prefer a location under
`~/Library/Application Support/Telos/`. Verify background directory access after
installation; successful foreground access alone does not establish service access.

Install copies the current executable into the directory, records its hash and exact
LaunchAgent configuration, and waits for a newly appended observation from a running
process. A new **error** record still proves the collector is working when its source
is offline. Status separates launchd `running`, `fresh_observation` and the last
observation; none alone proves a healthy window. This collector has no Codex task,
credential setup, mutable goal ledger or external-network dependency.

The agent runs finite million-sample sessions under launchd KeepAlive, so launchd
restarts it after a crash or a finished session. Samples are still bounded by the
64 MiB observation-file cap; reaching that limit prevents new writes, and repeated
restart cannot make a stale log fresh. There is no automatic rotation or log cleanup.
Inspect status and logs, remove the service before deliberate log maintenance, then
reinstall. The service continues writing errors when Telos is down but does not yet
notify independently; it does not prove a business service is healthy.

Duplicate install with identical settings is idempotent. Changed settings, modified
owned binaries and edited plists are refused. Remove confirms launchd absence and
released collector ownership before deleting the plist; observations, stdout/stderr,
manifest and executable remain. Reinstall uses that preserved executable. There is
no in-place collector upgrade yet: remove the old service and install a new build
in a new dedicated directory, retaining old evidence. Do not run overlapping
collectors against one output or delete ownership files to bypass checks.

Reproduce actual launchd start, crash/restart, preservation and reinstall checks:

```sh
python3 scripts/collector_service_smoke.py --output /private/tmp/new-telos-collector-check
```

The script uses an isolated offline source and removes its LaunchAgent in cleanup.
Its errors are real unreachable probes, not fabricated heartbeat records. Logout,
reboot, independent alerting and two-week monitoring acceptance remain separate.

## Historical observation coverage report

```sh
target/debug/telos collector report --directory /absolute/dedicated-collector \
  --minimum-duration-seconds 1209600 --max-gap-seconds 15
```

Defaults ask for fourteen days of records and no between-record gap above fifteen
seconds. They are report parameters, not a new goal contract or an uptime SLA.
The report separates observed span, `duration_met`, `coverage_complete` and
`all_samples_healthy`. Coverage may include error records: recording a failure is
still an observation. A stream with only recent healthy records cannot prove the
requested duration. `all_samples_healthy` alone does not prove freshness or coverage.

The read-only report counts errors, invalid/future records, timestamp regressions,
repeated timestamps, between-record gaps and the age of the last record. Empty,
partial, malformed or stale data cannot establish complete coverage. Coverage starts
at the first valid record in the current file; it does not infer history before it
or combine rotated archives. Counts are records, not independent health events.

It reads a bounded file prefix twice and verifies the same bytes and file identity,
without taking the collector's lock. Later appends are excluded; a captured partial
final line is reported rather than repaired. The prefix length and SHA256 identify
what was observed. This detects ordinary concurrent truncation/rewrite/rotation,
not arbitrary rewriting or authenticated provenance. Read failures return errors;
no source file, collector process or goal state is changed.

A complete control-interface coverage report is not business-service health,
continuous uptime between samples, logout/reboot acceptance or overall pilot
completion. The source-build command does not update the installed collector.

