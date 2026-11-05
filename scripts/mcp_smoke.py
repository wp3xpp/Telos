#!/usr/bin/env python3
"""Actual Codex -> production MCP bridge -> isolated daemon integration check."""
import argparse
import json
from pathlib import Path
import secrets
import shutil
import subprocess
import time

from phase0 import stop

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    state = output / 'state'
    binary = ROOT / 'target/debug/telos'
    report = {'status': 'running', 'checks': {}}
    codex = shutil.which('codex')
    assert codex
    daemon = None
    worker = None

    def call(request):
        path = output / 'operator-request.json'
        path.write_text(json.dumps(request))
        result = subprocess.run([str(binary), '--state-dir', str(state), 'request', str(path)],
                                capture_output=True, text=True, timeout=20)
        if result.returncode:
            raise RuntimeError(result.stderr)
        return json.loads(result.stdout)

    try:
        with (output / 'daemon.log').open('w') as log:
            daemon = subprocess.Popen([str(binary), '--state-dir', str(state), 'daemon'], stdout=log, stderr=log)
        for _ in range(100):
            try:
                call({'op': 'ping'})
                break
            except RuntimeError:
                assert daemon.poll() is None
                time.sleep(0.1)
        else:
            raise RuntimeError('daemon startup timeout')
        project = ROOT / 'tests/fixtures/codex-project'
        call({'op': 'register_project', 'id': 'fixture', 'root': str(project)})
        nonce = secrets.token_hex(16)
        record = call({'op': 'create_loop', 'project_id': 'fixture', 'name': 'MCP fixture', 'kind': 'delivery',
                       'contract': {'goal': 'Inspect fixture nonce ' + nonce, 'delivery_scope': 'fixture only',
                                    'criteria': [{'id': 'read', 'description': 'Read control-plane data'}],
                                    'interval_seconds': None, 'max_attempts': 1}})
        schema = {'type': 'object', 'properties': {'goal': {'type': 'string'}, 'state': {'type': 'string'}},
                  'required': ['goal', 'state'], 'additionalProperties': False}
        schema_path = output / 'schema.json'
        schema_path.write_text(json.dumps(schema))
        command = [codex, 'exec', '--ignore-user-config', '--ephemeral', '--skip-git-repo-check', '--json',
                   '--sandbox', 'read-only', '-C', str(project), '--output-schema', str(schema_path),
                   '-o', str(output / 'result.json'), '-c', 'mcp_servers.telos.command=' + json.dumps(str(binary)),
                   '-c', 'mcp_servers.telos.args=' + json.dumps(['--state-dir', str(state), 'mcp']),
                   '-c', 'mcp_servers.telos.required=true',
                   'Integration check: call the Telos loop_get MCP tool for id ' + record['id'] +
                   '. Return the exact contract goal and loop state from the actual tool response. '
                   'Do not read files, use shell tools, or mutate any goal.']
        with (output / 'events.jsonl').open('w') as stdout, (output / 'stderr.log').open('w') as stderr:
            worker = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, start_new_session=True)
            worker.wait(timeout=150)
        assert worker.returncode == 0, 'Codex execution failed; see stderr.log'
        result = json.loads((output / 'result.json').read_text())
        assert result == {'goal': record['contract']['goal'], 'state': 'active'}, result
        events = [json.loads(line) for line in (output / 'events.jsonl').read_text().splitlines()]
        assert any(e.get('item', {}).get('type') == 'mcp_tool_call' and
                   e['item'].get('tool') == 'loop_get' and e['item'].get('status') == 'completed' for e in events)
        report.update(status='passed', checks={'actual_codex_mcp_call': True, 'production_bridge': True,
                                              'daemon_state_returned': True, 'desktop_discovery': 'not_verified'})
    except Exception as exc:
        report.update(status='failed', error=str(exc))
        raise
    finally:
        if worker is not None:
            stop(worker)
        if daemon is not None:
            daemon.terminate()
            try:
                daemon.wait(timeout=15)
            except subprocess.TimeoutExpired:
                stop(daemon)
                report.update(status='failed', shutdown_error='daemon required forced termination')
            report['daemon_exit_code'] = daemon.returncode
            if daemon.returncode != 0:
                report['status'] = 'failed'
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))
    assert report['status'] == 'passed'


if __name__ == '__main__':
    main()
