# Telos v0.1 implementation contract

## Product

Codex remains the user interface and agent. Telos supplies a local Rust daemon,
stdio MCP bridge, hsw-telos skill, persistent SQLite ledger, bounded Codex runner,
and verification/scheduling. Loop is the product object; a cycle handles a single
observation/response/verification round. Delivery loops complete; continuous loops
return to observation. No Web UI, cloud runner, arbitrary desktop control, or
Codex-internal-database integration in v0.1.

The framework is project-neutral. Project-specific source formats, scripts,
environment protocols and acceptance adapters are integrations, not default Loop
semantics. See ADAPTER-ARCHITECTURE.md for the extension interfaces and supported
adapter types.

## Phases and acceptance

0. Integration: installed CLI/auth; real structured response and JSONL events;
   project instructions and memory read; real stdio MCP tool invocation;
   cancellation cleans owned processes; SSH/toolchain connectivity; desktop MCP
   discovery explicitly verified separately (CLI is not evidence of desktop wiring).
1. Ledger: projects, versioned contracts, loops, cycles, work items, steps, events,
   evidence and verification. Transactions, revision guards, restart persistence.
2. Runtime: durable queue/lease, bounded exec, structured results, timeout/cancel,
   restart reconciliation, no blind retries of uncertain side effects.
3. Codex UX: MCP create/list/get/update/pause/resume/stop/answer/evidence; skill;
   project inspection; installation/doctor; no internal worker controls exposed.
4. Monitoring: incremental log cursor, dedup, missing-data detection, diagnosis,
   scheduled verification, notification outbox. Historical fixtures then live pilot.
5. Delivery: isolated Git worktree, explicit task package, project-specific tests,
   existing environment lease, artifact/revision-bound verification; small real task.
6. Packaging/reliability: launchd, uninstall, compatibility, backup/restore, retention,
   fault injection, local notifications, two-week pilot observed rather than simulated.

## Invariants

- Only daemon writes the database; MCP routes through a private Unix socket.
- State/event/enqueued next step/outbox changes commit atomically.
- Verifications bind contract revision, artifact/environment and evidence.
- Missing observations, execution error, and outcome_unknown never mean success.
- On update, old results cannot advance the new contract.
- Pause prevents new work; in-flight remote effects require reconciliation.
- Task lease is distinct from the external test environment lock.
- Frontend and background must not own the same step concurrently.
- Standard MCP cannot self-grant new authority. New permissions require a trusted
  operator path; existing explicit project grants are respected.
- Agent shell work requires configured isolation; worktrees are not sandboxes.
- Saved local authentication does not guarantee unattended future availability.

## Initial architecture

Rust/Tokio, SQLx/SQLite WAL on local disk, JSON over private Unix socket, stdio MCP,
Codex exec JSONL plus output schema, TOML project config, tracing and clap.
Mac launchd first, Linux systemd subsequently. Database migrations and lockfile
are committed. Each executed task is bounded and owns one isolated directory.

## First workflows

- Service observation: observe -> aggregate -> diagnose -> associate remediation -> verify.
  Five-minute observation is not a replacement for real-time incident alerts.
- Feature delivery: contract -> plan -> develop -> project tests -> integration ->
  delivery verification. Preserve existing test/deploy scripts and their leases.

## Verified interface references

- https://learn.chatgpt.com/docs/non-interactive-mode
- https://learn.chatgpt.com/docs/extend/mcp?surface=cli
- https://developers.openai.com/plugins/concepts/skills

Local CLI help and live tests take precedence when docs and installed flags differ.
