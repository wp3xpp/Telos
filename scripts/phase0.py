#!/usr/bin/env python3
"""Live Codex contract test. Output contains fixture data only; never reads auth files."""
import argparse
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def process_table():
    rows = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,pgid=,stat=,lstart='], text=True)
    table = {}
    for row in rows.splitlines():
        parts = row.split(None, 4)
        if len(parts) == 5:
            table[int(parts[0])] = (int(parts[1]), int(parts[2]), parts[3], parts[4])
    return table


def stop(proc):
    # Codex can launch MCP servers in their own process groups. Snapshot the
    # descendant identities before terminating the parent, not after reparenting.
    table = process_table()
    owned = {proc.pid} if proc.poll() is None else set()
    while True:
        expanded = owned | {pid for pid, row in table.items() if row[0] in owned}
        if expanded == owned:
            break
        owned = expanded
    identities = {pid: table[pid][3] for pid in owned if pid in table}
    for sig in (signal.SIGTERM, signal.SIGKILL):
        current = process_table()
        for pid, started in identities.items():
            if pid in current and current[pid][3] == started:
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    pass
        if sig == signal.SIGTERM:
            time.sleep(0.3)
    try:
        proc.wait(timeout=8)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def alive(pid):
    row = process_table().get(pid)
    return row is not None and not row[2].startswith('Z')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--timeout', type=int, default=180)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    codex = shutil.which('codex')
    assert codex, 'codex not installed'
    report = {'checks': {}, 'started_at': time.time(), 'codex_version': subprocess.check_output([codex, '--version'], text=True).strip()}
    fields = ['probe_nonce', 'project_rule', 'memory_fact']
    schema = {'type': 'object', 'properties': {f: {'type': 'string'} for f in fields}, 'required': fields, 'additionalProperties': False}
    schema_path = output / 'result.schema.json'
    schema_path.write_text(json.dumps(schema))
    nonce = secrets.token_hex(16)
    try:
        for mode in ['inspect', 'cancel']:
            run = output / mode
            run.mkdir()
            # TOML strings and arrays are encoded individually, never interpolated into a shell.
            command = [codex, 'exec', '--ignore-user-config', '--ephemeral', '--skip-git-repo-check',
                       '--json', '--sandbox', 'read-only', '-C', str(ROOT / 'tests/fixtures/codex-project'),
                       '-c', 'mcp_servers.telos_probe.command=' + json.dumps(sys.executable),
                       '-c', 'mcp_servers.telos_probe.args=' + json.dumps([str(ROOT / 'scripts/probe_mcp.py'), '--evidence-dir', str(run), '--nonce', nonce]),
                       '-c', 'mcp_servers.telos_probe.required=true',
                       '-c', 'mcp_servers.telos_probe.tool_timeout_sec=180']
            if mode == 'inspect':
                command += ['--output-schema', str(schema_path), '-o', str(run / 'result.json')]
                prompt = 'Integration test only. Read AGENTS.md and MEMORY.md in this fixture. Call telos_probe probe_inspect once. Return the required JSON using the actual tool nonce, project_rule and memory_fact. Do not change files or inspect anything outside the fixture. Do not use other MCP tools.'
            else:
                prompt = 'Cancellation integration test only. Immediately call telos_probe probe_wait exactly once and wait for its result. Do not run any other tools or change any files.'
            command.append(prompt)
            with (run / 'events.jsonl').open('w') as stdout, (run / 'stderr.log').open('w') as stderr:
                proc = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, start_new_session=True)
                try:
                    deadline = time.monotonic() + args.timeout
                    if mode == 'inspect':
                        proc.wait(timeout=args.timeout)
                        assert proc.returncode == 0, f'inspect exit={proc.returncode}; inspect stderr.log'
                        result = json.loads((run / 'result.json').read_text())
                        assert result == {'probe_nonce': nonce, 'project_rule': 'fixture-read-only-v1', 'memory_fact': 'test-environment-uses-exclusive-lease'}, result
                        events = [json.loads(line) for line in (run / 'events.jsonl').read_text().splitlines()]
                        assert any(e['type'] == 'turn.completed' for e in events), 'no turn.completed'
                        calls = [json.loads(line) for line in (run / 'calls.jsonl').read_text().splitlines()]
                        assert any(c['tool'] == 'probe_inspect' for c in calls)
                        report['checks'].update(structured_result=True, event_stream=True, mcp_tool_call=True, project_instructions=True, project_memory=True)
                    else:
                        while not (run / 'wait.started').exists():
                            assert proc.poll() is None, 'cancel job exited before tool started'
                            assert time.monotonic() < deadline, 'cancel job never called probe_wait'
                            time.sleep(0.2)
                        start = time.monotonic()
                        stop(proc)
                        mcp_pid = int((run / 'mcp.pid').read_text())
                        for _ in range(50):
                            if not alive(mcp_pid):
                                break
                            time.sleep(0.1)
                        assert not alive(mcp_pid), f'MCP child survived cancellation: {mcp_pid}'
                        report['checks']['process_group_cancellation'] = True
                        report['cancellation_seconds'] = round(time.monotonic() - start, 3)
                finally:
                    stop(proc)
            print(f'{mode}: PASS', flush=True)
        report['status'] = 'passed'
    except Exception as exc:
        report['status'] = 'failed'
        report['error'] = str(exc)
        raise
    finally:
        report['finished_at'] = time.time()
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2), flush=True)


if __name__ == '__main__':
    main()
