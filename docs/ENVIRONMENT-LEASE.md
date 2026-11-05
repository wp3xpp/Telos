# Project-owned environment coordination

Shared environments need a project-specific adapter. Implement the
`ProjectTestAdapter` interface described in [adapter architecture](ADAPTER-ARCHITECTURE.md).
The adapter owns its transport, coordination protocol, credentials and cleanup.
Telos does not bundle a provider, SSH target, deployment script or environment path.

## Framework responsibilities

The framework records execution intent before dispatch and confirmed outcome
thereafter, binds operations to a goal revision and verification owner, and fences
uncertain ownership. A task queue lease and an external environment lease represent
different resources; neither substitutes for the other.

The Rust library's `EnvironmentAdapterGrant` contains only `enabled`,
`environment_sha256` and `adapter_sha256`. Trusted operator or embedding code uses
`Store::configure_environment_adapter` to record that explicit authority under an
existing project write policy. The adapter computes its fingerprints from its own
identity and configuration and verifies them before dispatch. Framework code does
not interpret hosts, script names or provider schemas. This grant API is not exposed
as an agent MCP tool or a provider-configuring CLI command.

`EnvironmentIntent` binds operations to the grant key/version, fingerprints,
session, request key and optional verification lease. `begin_environment_operation`
commits the intent before the adapter calls its provider; `finish_environment_operation`
records a confirmed result. These library methods do not perform external actions.

## Uncertain outcomes

A lost response, timeout or interrupted operation must remain unknown. An unknown
ownership record blocks conflicting work. A successful local process cleanup does
not prove that a remote operation stopped or that its resource was released.
The project adapter must inspect and reconcile the provider through its own
protocol before asking Telos to retire a local uncertainty fence.

Normal operator request `reconcile_environment` accepts a revision-bound
`EnvironmentReconciliation` after the owning goal is paused and its operations
are independently stopped. It checks current journal sequence/version and fresh
operator attestations. It does not contact a provider, force-release resources,
resume the goal or accept a delivery. Ordinary MCP does not expose this operation.

## Restored state

Backups retain environment ownership history. Recovery review and activation
remain blocked by unresolved sessions; imported grants require reauthorization.
For a stopped recovery image, `backup reconcile-environment` creates a separate
reviewed copy using hash-bound operator materials and a unique decision. The
source remains unchanged and goals remain paused.

See [installation and recovery](INSTALL.md#reconcile-an-external-environment-session-in-a-recovery-copy)
for the operator request format. Journal and recovery tests use synthetic
fingerprints and sessions. Live external coordination and remote artifact
attestation require separate project integration and operational validation.
