# The local delivery main flow

1. State the request and observable acceptance criteria in Codex.
2. The frontend uses the registered project's existing scope and execution policy.
3. Telos pins a committed base and prepares an owned worktree.
4. A background Codex worker implements the request.
5. Independent operator-configured sandbox checks evaluate the observed artifact.
6. Failed checks may enqueue a correction within the existing attempt budget.
7. Passed criteria complete the delivery and preserve an artifact-bound review patch.
8. The frontend reviews and applies that patch, then checks the original project.

Use `deliver --file PACKAGE --wait`, `follow --id ID`, and
`review --id ID --output /absolute/new-directory` as operator shortcuts.
Users express requirements in chat; the frontend prepares the package.

See DAILY-USE.md for examples, DELIVERY.md for workspace semantics, TESTING.md for
acceptance configuration, and REPAIR.md for automatic correction boundaries.
