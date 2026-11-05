# Reproduce a complete delivery

After installation and service setup, run a fresh isolated demo project:

```sh
python3 scripts/user_flow_demo.py --adapter local.sandbox_json \
  --output /absolute/new/demo-output
```

This discovers the installed Telos client through the public Codex MCP
registration. It registers an isolated demo project, submits a Markdown local-link
checker requirement, runs a real background Codex worker and six independent
sandbox tests, and exports a saved patch/evidence bundle.

It uses Python 3, Node.js, an authenticated Codex CLI and model quota. It creates
one durable demo goal; follow its saved ID rather than resubmitting after a timeout.
The exported patch is review-only. Inspect it before applying it to the demo's
original project, and rerun its external acceptance script afterward.

For a controlled correction exercise, `scripts/repair_smoke.py` uses a synthetic
incomplete first attempt followed by a real Codex repair. It is a local fixture,
not a production incident test.
