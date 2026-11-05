#!/usr/bin/env python3
"""Actual daemon -> queue -> Codex -> evidence smoke. No project modifications."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--managed', action='store_true')
    parser.add_argument('--verify', action='store_true', help='Verify unchanged fixture bytes independently after execution')
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    binary = ROOT / 'target/debug/telos'
    state = root / 'state'
    report = {'checks': {}, 'status': 'running'}
    request_index = 0

    def call(request):
        nonlocal request_index
        request_index += 1
        path = root / f'request-{request_index}.json'
        path.write_text(json.dumps(request))
        result = subprocess.run([str(binary), '--state-dir', str(state), 'request', str(path)],
                                capture_output=True, text=True, timeout=20)
        if result.returncode:
            raise RuntimeError(result.stderr.strip())
        return json.loads(result.stdout)

    def audit_events(loop_id):
        request = {'op': 'events', 'id': loop_id}
        items = []
        while True:
            page = call(request)
            items.extend(page['items'])
            if page['next_after_sequence'] is None:
                return items
            request.update(after_sequence=page['next_after_sequence'], through_sequence=page['through_sequence'])

    with (root / 'daemon.log').open('w') as log:
        proc = subprocess.Popen([str(binary), '--state-dir', str(state), 'daemon', '--codex', shutil.which('codex')], stdout=log, stderr=log)
        try:
            for _ in range(100):
                try:
                    call({'op': 'ping'})
                    break
                except RuntimeError:
                    assert proc.poll() is None, 'daemon exited during startup'
                    time.sleep(0.1)
            else:
                raise RuntimeError('daemon startup timed out')
            project = ROOT / 'tests/fixtures/codex-project'
            call({'op': 'register_project', 'id': 'fixture', 'root': str(project)})
            loop = call({'op': 'create_loop', 'project_id': 'fixture', 'name': 'Actual Codex read-only smoke', 'kind': 'delivery', 'contract': {
                'goal': 'Read synthetic project AGENTS.md and MEMORY.md. Include the exact memory_fact value in the result summary.', 'delivery_scope': 'isolated fixture',
                'criteria': [{'id': 'memory', 'description': 'MEMORY.md retains its exact original fixture bytes' if args.verify else 'Synthetic memory read with execution evidence'}],
                'interval_seconds': None, 'max_attempts': 1}})
            if args.verify:
                call({'op': 'configure_verification', 'id': loop['id'], 'revision': 1, 'expected_version': 0,
                      'plan': {'checks': [{'criterion_id': 'memory', 'relative_path': 'MEMORY.md',
                                           'expected_sha256': hashlib.sha256((project / 'MEMORY.md').read_bytes()).hexdigest()}]}})
            if args.managed:
                call({'op': 'configure_project', 'id': 'fixture', 'expected_revision': 0, 'policy': {
                    'enabled': True, 'sandbox': 'read_only', 'timeout_seconds': 150,
                    'max_output_bytes': 1048576, 'max_steps_per_cycle': 1}})
                start = {'op': 'start_loop', 'id': loop['id'], 'revision': 1, 'request_key': 'inspect'}
                queued = call(start)
                repeated = call(start)
                assert repeated['step_id'] == queued['step_id'] and repeated['cycle_id'] == queued['cycle_id'], 'start retry must not enqueue duplicate work'
            else:
                queued = call({'op': 'enqueue', 'id': loop['id'], 'revision': 1, 'trigger_key': 'smoke', 'request_key': 'inspect', 'spec': {
                'task': 'Read only AGENTS.md and MEMORY.md from this fixture. Include the exact memory_fact value in summary. Return status done, evidence_paths ["AGENTS.md", "MEMORY.md"], remaining_work []. Do not change files or inspect other directories.',
                'cwd': str(project), 'sandbox': 'read_only', 'timeout_seconds': 150, 'max_output_bytes': 1048576}})
            deadline = time.monotonic() + 180
            while time.monotonic() < deadline:
                assert proc.poll() is None, 'daemon exited while running job'
                step = call({'op': 'step_status', 'id': queued['step_id']})
                if step['state'] not in ('ready', 'running'):
                    break
                time.sleep(0.5)
            else:
                raise RuntimeError('job timeout')
            assert step['state'] == 'succeeded', step
            assert 'test-environment-uses-exclusive-lease' in step['result']['result']['summary'], step
            after = call({'op': 'get_loop', 'id': loop['id']})
            if args.verify:
                for _ in range(50):
                    after = call({'op': 'get_loop', 'id': loop['id']})
                    if after['state'] == 'completed':
                        break
                    time.sleep(0.1)
                assert after['state'] == 'completed', 'independent verification did not close fixture goal'
            else:
                assert after['state'] == 'active', 'agent completion must not close goal'
            events = audit_events(loop['id'])
            assert any(e['kind'] == 'evidence_recorded' for e in events)
            if args.verify:
                assert any(e['kind'] == 'verification_recorded' for e in events)
            report.update(status='passed', loop_id=loop['id'], step_id=queued['step_id'], checks={
                'private_daemon': True, 'durable_queue': True, 'actual_codex_job': True,
                'project_memory': True, 'execution_evidence': True, 'no_false_goal_completion': True})
            report['mode'] = 'policy_managed' if args.managed else 'operator'
            if args.verify:
                report['checks']['independent_file_acceptance'] = True
                report['verification_scope'] = 'Exact original MEMORY.md fixture bytes; not general software correctness'
        except Exception as exc:
            report.update(status='failed', error=str(exc))
            raise
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
                report['shutdown_error'] = 'daemon did not exit gracefully'
                report['status'] = 'failed'
            report['daemon_exit_code'] = proc.returncode
            if proc.returncode != 0:
                report['status'] = 'failed'
            (root / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(report, indent=2))
        if report['status'] != 'passed':
            raise RuntimeError('runtime smoke failed; see report.json')


if __name__ == '__main__':
    main()
