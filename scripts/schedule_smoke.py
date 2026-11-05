#!/usr/bin/env python3
"""Actual wall-clock daemon recurrence with an explicitly synthetic Codex executable."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--log-window', action='store_true', help='Observe a real-time heartbeat/error recovery window')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    project = output / 'project'
    project.mkdir()
    (project / 'value').write_bytes(b'healthy fixture')
    fake = output / 'fake-codex'
    fake.write_text('''#!/usr/bin/env python3
import json,pathlib,sys
sys.stdin.read()
pathlib.Path(sys.argv[sys.argv.index('-o')+1]).write_text(json.dumps({'status':'done','summary':'Synthetic observation','evidence_paths':[],'remaining_work':[]}))
print(json.dumps({'type':'turn.completed','usage':{'output_tokens':0}}))
''')
    fake.chmod(0o700)
    binary = ROOT / 'target/debug/telos'
    state = output / 'state'
    report = {'status': 'running', 'executor': 'synthetic', 'clock': 'real'}

    def call(request):
        path = output / 'request.json'
        path.write_text(json.dumps(request))
        result = subprocess.run([str(binary), '--state-dir', str(state), 'request', str(path)], capture_output=True, text=True, timeout=20)
        if result.returncode:
            raise RuntimeError(result.stderr)
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

    with (output / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([str(binary), '--state-dir', str(state), 'daemon', '--codex', str(fake)], stdout=log, stderr=log)
    started = time.monotonic()
    try:
        for _ in range(100):
            try:
                call({'op': 'ping'})
                break
            except RuntimeError:
                assert daemon.poll() is None
                time.sleep(0.1)
        else:
            raise RuntimeError('daemon startup timeout')
        call({'op': 'register_project', 'id': 'fixture', 'root': str(project)})
        call({'op': 'configure_project', 'id': 'fixture', 'expected_revision': 0, 'policy': {
            'enabled': True, 'sandbox': 'read_only', 'timeout_seconds': 10, 'max_output_bytes': 65536, 'max_steps_per_cycle': 1}})
        goal = call({'op': 'create_loop', 'project_id': 'fixture', 'name': 'Periodic fixture', 'kind': 'delivery' if args.log_window else 'continuous', 'contract': {
            'goal': 'Observe a complete healthy heartbeat window after transient errors' if args.log_window else 'Observe unchanged fixture bytes', 'delivery_scope': 'fixture', 'criteria': [{'id': 'bytes', 'description': 'Ten seconds of fresh heartbeats without errors' if args.log_window else 'Exact fixture content'}],
            'interval_seconds': None if args.log_window else 30, 'max_attempts': 1}})
        plan = {'checks': [{'criterion_id': 'bytes', 'relative_path': 'value', 'expected_sha256': hashlib.sha256(b'healthy fixture').hexdigest()}]}
        if args.log_window:
            plan = {'log_checks': [{'criterion_id': 'bytes', 'relative_path': 'events.jsonl', 'window_seconds': 10, 'max_gap_seconds': 10}]}
        call({'op': 'configure_verification', 'id': goal['id'], 'revision': 1, 'expected_version': 0, 'plan': plan})
        call({'op': 'start_loop', 'id': goal['id'], 'revision': 1, 'request_key': 'user-start'})
        source_started = time.monotonic()
        injected_error = None
        while time.monotonic() - started < 60:
            assert daemon.poll() is None
            if args.log_window:
                timestamp = int(time.time())
                with (project / 'events.jsonl').open('a') as source:
                    source.write(json.dumps({'timestamp': timestamp, 'kind': 'heartbeat'}) + '\n')
                    if injected_error is None and time.monotonic() - source_started >= 3:
                        source.write(json.dumps({'timestamp': timestamp, 'kind': 'error', 'message': 'synthetic transient error'}) + '\n')
                        injected_error = timestamp
            events = audit_events(goal['id'])
            passed = [e for e in events if e['kind'] == 'verification_recorded' and e['body']['verdict'] == 'passed']
            if len(passed) >= (1 if args.log_window else 2):
                break
            time.sleep(0.5)
        else:
            raise RuntimeError('second periodic verification never arrived')
        current = call({'op': 'get_loop', 'id': goal['id']})
        if args.log_window:
            assert current['state'] == 'completed'
            assert injected_error is not None and passed[0]['created_at'] >= injected_error + 10
            assert any(e['kind'] == 'verification_recorded' and e['body']['verdict'] == 'failed' for e in events)
            report.update(status='passed', window_seconds=10, elapsed_seconds=round(time.monotonic()-started, 2),
                          checks={'incremental_observer': True, 'error_detected': True, 'complete_recovery_window': True, 'goal_completed_after_evidence': True})
        else:
            assert len(passed) == 2
            assert passed[1]['created_at'] - passed[0]['created_at'] >= 30
            assert current['state'] == 'active' and current['next_due_at'] > time.time()
            paused = call({'op': 'control', 'id': goal['id'], 'expected_version': current['version'], 'action': 'pause'})
            assert paused['state'] == 'paused'
            report.update(status='passed', cycles_verified=2, interval_seconds=30, elapsed_seconds=round(time.monotonic()-started, 2),
                          checks={'real_timer_recurs': True, 'independent_acceptance_each_cycle': True, 'continuous_stays_active': True, 'pause_accepted': True})
    except Exception as exc:
        report.update(status='failed', error=str(exc))
        raise
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=15)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait()
            report.update(status='failed', shutdown_error='forced termination')
        report['daemon_exit_code'] = daemon.returncode
        if daemon.returncode != 0:
            report['status'] = 'failed'
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))
    assert report['status'] == 'passed'


if __name__ == '__main__':
    main()
