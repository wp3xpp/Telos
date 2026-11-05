# Capabilities and support status

Telos v0.1 supports local development on macOS. Codex provides the conversation
and agent; Telos manages persistent goals, bounded execution and independent
acceptance. Start with the [README](../README.md) and [daily-use guide](DAILY-USE.md).

## Available workflows

- Delivery: versioned requirements, an isolated Git worktree, background Codex work,
  independent sandbox checks, bounded correction, and a saved patch for review.
- Continuous observation: scheduled cycles, read-only inspection and diagnosis,
  collection cursors, deduplicated signals and independent health checks.
- Readiness: inspect configuration gaps, collection receipts, scheduling and the
  first recorded verdict for an existing Loop with `telos readiness --id LOOP_ID`.
- Inspection recovery: collection receipts remain available for blocked goals.
  Operators can reconcile an eligible historical read-only output truncation.
- Recovery: link a diagnosis to a separate delivery, record a project action receipt,
  then require a fresh observation window for the measured action subject.
- Operations: stdio MCP, a local daemon, macOS launchd services, local notifications,
  offline backup verification and explicit recovery-review commands.

## Adapter support

| Adapter | Purpose |
| --- | --- |
| `local.sandbox_command` | Delivery acceptance using a configured command's exit status |
| `local.sandbox_json` | Delivery acceptance using a configured command and structured assertion |
| `local.observation_json` | Continuous acceptance using bound observations, complete coverage and fresh measurements |

Project adapters supply source transport, log parsing, business predicates and
action drivers. See [adapter architecture](ADAPTER-ARCHITECTURE.md) and the
[project observation protocol](PROJECT-OBSERVATION-PROTOCOL.md).

Continuous workers run read-only. Native observation probes deny writes, network
access and undeclared input reads; this probe backend currently requires macOS.
Dynamic plugin loading is not implemented. Source collection, format normalization
and external environment providers are project-owned implementations; the framework
does not bundle business integrations.

## Validation

The framework regression suite contains 151 passing tests. Formatting, Clippy
with warnings denied, and a locked build also pass. Run the commands in the README
to verify your checkout.
[TESTING.md](TESTING.md) describes reproducible integration harnesses and their
prerequisites. The generic observation harness uses a disposable project, synthetic
worker/action driver, native observation sandbox and actual Codex sandbox delivery
checks. It verifies that delivery completion alone cannot establish recovery.
The correction harness also verifies a synthetic incomplete first attempt followed
by actual Codex correction, independent sandbox acceptance and a saved review patch.
These checks exercise framework behavior in disposable projects. They do not
establish production health or deployment readiness.

## Remaining scope

Unattended production remediation, independent remote shared-environment artifact
attestation, repeated restore generations, and long-duration/logout/reboot
reliability require further implementation or operational validation. See
[remaining acceptance](ACCEPTANCE-GAPS.md). Adding an adapter or recording a goal
does not grant execution permissions or arm its schedule.
