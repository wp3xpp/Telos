# Telos engineering rules

Telos extends Codex with persistent goal contracts, bounded execution, and
evidence-based verification. The accepted scope is in docs/IMPLEMENTATION.md.

- Use Rust for the product. Python scripts are integration-test harnesses only.
- Never read, copy, print, or commit Codex/SSH credentials.
- Never mutate Codex internal databases or infer desktop availability from CLI success.
- Read-only probes must not modify production, test runtimes, or other projects.
- Every state mutation includes an audit event in the same transaction.
- Worker success is not verification success. Missing evidence is inconclusive.
- Lease expiry cannot authorize an unknown external action to be repeated.
- Contract revision invalidates stale plans, results, and verification.
- Existing user/project authorizations persist; never manufacture approvals.
- Keep test evidence reproducible and distinguish synthetic fixtures from live checks.
- Maintain docs/STATUS.md with supported capabilities and remaining acceptance criteria.
- Keep project-specific collectors, log parsers, provider scripts and business
  acceptance rules outside this repository. Use neutral fixtures to test generic
  interfaces; adding a project must not add a business-specific core branch.
- Write public documentation for framework users. Keep personal project names, local
  installation diaries and unpublished evidence references out of product docs.
  Describe adapter contracts and reproducible examples instead.

Build and test Telos locally on macOS using the committed Cargo workspace and lockfile.
