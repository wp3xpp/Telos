# Remaining acceptance

The local delivery flow is validated: scoped request, real background Codex work,
independent sandbox verification, bounded correction, saved patch review, and
frontend application/retesting. Installation and desktop MCP calls were exercised.

Remaining work includes independent remote shared-environment artifact/process
attestation, production diagnosis/remediation/recovery acceptance, operational
restore and repeated recovery pilots, and long-duration/logout/reboot reliability.

A successful worker run is not goal acceptance. Acquisition success is not business
health. Unknown external outcomes retain ownership and require reconciliation.
See IMPLEMENTATION.md and ADAPTER-ARCHITECTURE.md for the framework boundaries.


The generic observation/recovery fixture now passes, including bounded collection,
physical episode deduplication, action-subject binding and a fresh post-action
window. This does not implement a particular project's collector, business health
rules, deployment or final commit/push coordinator. Those belong to the project
adapter and must be exercised against that project's real environment. The native
read-only observation probe currently supports macOS only.
