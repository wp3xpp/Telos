#!/usr/bin/env python3
"""Real Codex diagnosis/delivery and independent recovery of a local Rust fixture.

Opt-in integration harness, not a production incident or deployment. All ledger
changes use the isolated daemon. Only the exact fixture repair is integrated after
checking its saved patch and independently tested workspace.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
INITIAL = '''pub fn event_kind(ready: bool) -> &'static str {
    if ready { "error" } else { "error" }
}
fn main() {
    let ready = std::env::args().nth(1).as_deref() == Some("ready");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    println!("{{\\"timestamp\\":{},\\"kind\\":\\"{}\\"}}", timestamp, event_kind(ready));
}
'''
REPAIRED = INITIAL.replace('if ready { "error" }', 'if ready { "heartbeat" }')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--codex', default=shutil.which('codex'))
    parser.add_argument('--continue-delivery', action='store_true', help='Continue a retained completed delivery before integration; never rerun its model task')
    parser.add_argument('--rustc', type=Path, required=True, help='Actual compiler under a toolchain, not a rustup shim')
    args = parser.parse_args()
    assert args.codex
    rustc = args.rustc.resolve()
    output = args.output.resolve()
    previous = None
    if args.continue_delivery:
        previous_text = (output / 'report.json').read_text()
        previous = json.loads(previous_text)
        assert previous['daemon_exit_code'] == 0
        assert previous['delivery']['verdict'] == 'passed'
        assert not (output / 'before-delivery-continuation.json').exists()
        (output / 'before-delivery-continuation.json').write_text(previous_text)
        state = Path(previous['state_dir'])
    else:
        output.mkdir(parents=True, exist_ok=False)
        state = Path(tempfile.mkdtemp(prefix='telos-recovery-', dir='/tmp')) / 'state'
    project = output / 'project'
    acceptance = output / 'acceptance'
    test = acceptance / 'contract.rs'
    if not args.continue_delivery:
        project.mkdir()
        (project / 'service.rs').write_text(INITIAL)
        (project / '.gitignore').write_text('observations.jsonl\n')
        (project / 'AGENTS.md').write_text(
            'This is a local Rust fixture. Read MEMORY.md. Read-only tasks must not edit.\n'
            'For the delivery task change only the ready=true branch in service.rs '
            'from error to heartbeat; preserve every other byte and do not reformat.\n'
            'Do not build, test, commit, access other projects, or use the network. '
            'Independent tests run after delivery. No production service is involved.\n')
        (project / 'MEMORY.md').write_text(
            'ready=true must emit a heartbeat. ready=false must emit an error.\n'
            'The harness runs the compiled fixture with ready and appends its actual '
            'timestamped output to observations.jsonl every second. Recovery requires '
            'a fresh independent ten-second heartbeat window.\n')
        acceptance.mkdir()
        test.write_text(
            'mod service { include!(concat!(env!("TELOS_TEST_WORKSPACE"), "/service.rs")); }\n'
            '#[test] fn ready_produces_heartbeat() { assert_eq!(service::event_kind(true), "heartbeat"); }\n'
            '#[test] fn not_ready_remains_error() { assert_eq!(service::event_kind(false), "error"); }\n')
    binary = ROOT / 'target/debug/telos'
    app = output / 'fixture-app'
    if not args.continue_delivery:
        subprocess.run([str(rustc), str(project / 'service.rs'), '-o', str(app)], check=True, capture_output=True)

    def git(*command):
        return subprocess.run(['git', '-C', str(project), '-c', 'user.name=Telos Fixture',
            '-c', 'user.email=fixture@example.invalid', '-c', 'commit.gpgSign=false',
            '-c', 'core.hooksPath=/dev/null', *command], check=True, capture_output=True, timeout=20)

    if not args.continue_delivery:
        git('init', '--quiet')
        git('add', '.')
        git('commit', '-qm', 'Local service fixture with a known heartbeat bug')
    report = {'status': 'running', 'scope': 'local Rust fixture; no production incident or deployment',
              'executor': 'real_codex', 'state_dir': str(state), 'started_at': int(time.time()),
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
    producer_stop = threading.Event()
    producer_errors = []

    def produce():
        try:
            while not producer_stop.is_set():
                sample = subprocess.check_output([str(app), 'ready'], text=True, timeout=5)
                event = json.loads(sample)
                assert event['kind'] in ('heartbeat', 'error')
                assert abs(int(time.time()) - event['timestamp']) <= 2
                with (project / 'observations.jsonl').open('a') as log:
                    log.write(sample)
                producer_stop.wait(1)
        except Exception as error:
            producer_errors.append(str(error))

    def call(request):
        path = output / 'request.json'
        path.write_text(json.dumps(request))
        result = subprocess.run([str(binary), '--state-dir', str(state), 'request', str(path)],
                                capture_output=True, text=True, timeout=90)
        if result.returncode:
            raise RuntimeError(result.stderr)
        return json.loads(result.stdout)

    def audit(loop_id):
        request = {'op': 'events', 'id': loop_id, 'limit': 20}
        items = []
        while True:
            page = call(request)
            items.extend(page['items'])
            if page['next_after_sequence'] is None:
                return items
            request.update(after_sequence=page['next_after_sequence'], through_sequence=page['through_sequence'])

    def wait_for(predicate, seconds=240):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            assert daemon.poll() is None
            assert not producer_errors, producer_errors
            result = predicate()
            if result:
                return result
            time.sleep(1)
        raise TimeoutError('Pilot observation timed out; inspect retained state before repeating any work')

    def policy(expected, sandbox):
        return call({'op': 'configure_project', 'id': 'fixture', 'expected_revision': expected,
                     'policy': {'enabled': True, 'sandbox': sandbox, 'timeout_seconds': 180,
                                'max_output_bytes': 262144, 'max_steps_per_cycle': 4}})

    def context(loop_id):
        return call({'op': 'loop_context', 'id': loop_id, 'revision': 1})

    def control(loop_id, action):
        current = call({'op': 'get_loop', 'id': loop_id})
        return call({'op': 'control', 'id': loop_id, 'expected_version': current['version'], 'action': action})

    def evidence(evidence_id):
        data = bytearray()
        offset = 0
        while True:
            page = call({'op': 'get_evidence', 'id': evidence_id, 'offset': offset, 'limit': 32768})
            data.extend(page['bytes'])
            if page['next_offset'] is None:
                return bytes(data)
            offset = page['next_offset']

    with (output / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([str(binary), '--state-dir', str(state), 'daemon', '--codex', args.codex],
                                  stdout=log, stderr=log)
    report['daemon_pid'] = daemon.pid
    producer = threading.Thread(target=produce)
    producer.start()
    parent = child = None
    started = time.monotonic()
    try:
        for _ in range(100):
            try:
                call({'op': 'ping'})
                break
            except RuntimeError:
                assert daemon.poll() is None
                time.sleep(.1)
        else:
            raise RuntimeError('daemon startup timeout')
        if args.continue_delivery:
            parent = call({'op': 'get_loop', 'id': previous['parent_id']})
            child = call({'op': 'get_loop', 'id': previous['child_id']})
            assert parent['state'] == 'paused' and child['state'] == 'completed'
            inspected = call({'op': 'inspect_project', 'id': 'fixture'})
            assert Path(inspected['root']).resolve() == project.resolve()
            assert inspected['execution_policy']['revision'] == 2
            assert inspected['execution_policy']['policy']['sandbox'] == 'workspace_write'
            idle = call({'op': 'verification_status', 'id': child['id']})
            assert idle['active_verification'] is None
            assert not idle['external_environment']['new_execution_blocked']
            workspace = call({'op': 'workspace_status', 'id': child['id'], 'revision': 1})
            report.update(parent_id=parent['id'], child_id=child['id'],
                          diagnosis_step=previous['diagnosis_step'], association=previous['association'],
                          checkpoint_continuation=True,
                          prior_report_sha256=hashlib.sha256(previous_text.encode()).hexdigest())
            print('Continuing retained verified delivery; no diagnosis or code repair repeated.', flush=True)
        else:
            call({'op': 'register_project', 'id': 'fixture', 'root': str(project)})
            policy(0, 'read_only')
            parent = call({'op': 'create_loop', 'project_id': 'fixture', 'kind': 'continuous',
                'name': 'Observe and independently verify local fixture recovery', 'contract': {
                'goal': 'Observe the local Rust fixture; diagnose errors and keep monitoring after independent recovery.',
                'delivery_scope': 'Read-only fixture investigation. Do not edit or deploy; remediation is a separate delivery.',
                'criteria': [{'id': 'fresh-heartbeats', 'description': 'Actual local fixture emits fresh heartbeats for ten seconds without errors'}],
                'interval_seconds': 30, 'max_attempts': 4}})
            report['parent_id'] = parent['id']
            (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
            call({'op': 'configure_verification', 'id': parent['id'], 'revision': 1, 'expected_version': 0,
                'plan': {'log_checks': [{'criterion_id': 'fresh-heartbeats', 'relative_path': 'observations.jsonl',
                                        'window_seconds': 10, 'max_gap_seconds': 10}]}})
            call({'op': 'start_loop', 'id': parent['id'], 'revision': 1, 'request_key': 'observe-before-repair'})
            print('Real observation and failure diagnosis started.', flush=True)
            def diagnosed():
                current = call({'op': 'get_loop', 'id': parent['id']})
                if current['state'] == 'blocked':
                    raise RuntimeError('Managed inspection/diagnosis blocked; inspect context before a new attempt: '
                                       + json.dumps(context(parent['id'])))
                rows = audit(parent['id'])
                return rows if any(e['kind'] == 'diagnosis_ready' for e in rows) else None

            before = wait_for(diagnosed)
            assert any(e['kind'] == 'verification_recorded' and e['body']['verdict'] == 'failed' for e in before)
            diagnosis_id = next(e['body']['step_id'] for e in before if e['kind'] == 'diagnosis_ready')
            report['diagnosis_step'] = call({'op': 'step_status', 'id': diagnosis_id})
            assert report['diagnosis_step']['state'] == 'succeeded'
            control(parent['id'], 'pause')
            policy(1, 'workspace_write')  # Explicit fixture operator grant, never self-granted by MCP.
            child = call({'op': 'create_loop', 'project_id': 'fixture', 'kind': 'delivery',
                'name': 'Repair local fixture heartbeat branch', 'contract': {
                'goal': 'In service.rs change only the ready=true return literal from error to heartbeat. '
                        'Keep ready=false returning error. Preserve all other bytes; do not reformat.',
                'delivery_scope': 'Only service.rs in assigned worktree. No builds/tests/commits/network or deployment.',
                'criteria': [{'id': 'mapping', 'description': 'Independent Rust tests assert both ready=true and ready=false behavior'}],
                'interval_seconds': None, 'max_attempts': 2}})
            report['child_id'] = child['id']
            association = call({'op': 'associate_remediation', 'id': parent['id'], 'revision': 1,
                'diagnosis_step_id': diagnosis_id, 'delivery_loop_id': child['id'], 'delivery_revision': 1,
                'request_key': 'fixture-remediation'})
            report['association'] = association
            command = {'program': '/bin/bash', 'args': ['-c',
                "set -e\ntrap 'rm -f target/contract-tests; rmdir target 2>/dev/null || true' EXIT\n"
                'export TELOS_TEST_WORKSPACE="$PWD"\nmkdir -p target\n'
                + shlex.quote(str(rustc)) + ' --test ' + shlex.quote(str(test)) + ' -o target/contract-tests\ntarget/contract-tests\n'],
                'readable_roots': [str(rustc.parent.parent), str(acceptance), '/Library/Developer/CommandLineTools',
                                   '/usr/bin', '/bin', '/usr/lib', '/System/Library'],
                'timeout_seconds': 60, 'max_output_bytes': 65536}
            call({'op': 'configure_verification', 'id': child['id'], 'revision': 1, 'expected_version': 0,
                  'plan': {'test_checks': [{'criterion_id': 'mapping', 'command': command}]}})
            workspace = call({'op': 'prepare_workspace', 'id': child['id'], 'revision': 1, 'policy_revision': 2, 'base': 'HEAD'})
            call({'op': 'start_loop', 'id': child['id'], 'revision': 1, 'request_key': 'repair-fixture-once'})
            print('Diagnosis saved; real code repair queued in independent worktree.', flush=True)
            wait_for(lambda: call({'op': 'get_loop', 'id': child['id']})['state'] == 'completed')
        delivery = call({'op': 'delivery_result', 'id': child['id'], 'revision': 1})
        assert delivery['verdict'] == 'passed'
        report['delivery'] = delivery
        review = json.loads(evidence(delivery['review_evidence_id']))
        assert review['format'] == 'telos-delivery-review-v1'
        assert delivery['artifact'] == 'workspace:' + review['workspace_id'] + ':' + review['snapshot_sha256']
        (output / 'review-evidence.json').write_text(json.dumps(review, indent=2) + '\n')
        (output / 'reviewed.patch').write_text(review['patch'])
        # This explicit fixture review admits one known literal change only. It is
        # not a general automatic merge of model-generated patches.
        assert (project / 'service.rs').read_text() == INITIAL
        assert (Path(workspace['path']) / 'service.rs').read_text() == REPAIRED
        patch_path = output / 'reviewed.patch'
        numstat = git('apply', '--numstat', str(patch_path)).stdout.decode().splitlines()
        assert len(numstat) == 1 and numstat[0].split('\t')[-1] == 'service.rs', numstat
        git('apply', '--check', str(patch_path))
        git('apply', str(patch_path))
        assert (project / 'service.rs').read_text() == REPAIRED
        report['reviewed_integration'] = {'only_service_rs_changed': True, 'exact_known_literal_repair': True,
                                         'source_sha256': hashlib.sha256((project / 'service.rs').read_bytes()).hexdigest()}
        next_app = output / 'fixture-app-repaired'
        subprocess.run([str(rustc), str(project / 'service.rs'), '-o', str(next_app)], check=True, capture_output=True)
        os.replace(next_app, app)
        report['repaired_app_started_at'] = int(time.time())
        policy(2, 'read_only')
        control(parent['id'], 'resume')
        call({'op': 'start_loop', 'id': parent['id'], 'revision': 1, 'request_key': 'observe-after-reviewed-repair'})
        print('Reviewed repair running. Parent independently observes a fresh ten-second heartbeat window.', flush=True)

        def recovered():
            current = context(parent['id'])
            links = current['remediation_links']['items']
            return current if links and links[0]['recovery_check_current'] and links[0]['recovery_check']['verdict'] == 'passed' else None

        final = wait_for(recovered)
        schedule = call({'op': 'schedule_status', 'id': parent['id']})
        assert schedule['armed'] is True
        assert call({'op': 'get_loop', 'id': parent['id']})['state'] == 'active'
        link = final['remediation_links']['items'][0]
        assert link['recovery_check']['verification_started_sequence'] > link['event_sequence']
        assert all(r['verdict'] == 'passed' for r in final['latest_verification']['results'])
        followed = subprocess.run([str(binary), '--state-dir', str(state), 'follow', '--id', parent['id'], '--once'],
                                  capture_output=True, text=True, check=True, timeout=20)
        report['follow_snapshot'] = json.loads(followed.stdout)
        assert report['follow_snapshot']['state'] == 'observing'
        report.update(status='passed', elapsed_seconds=round(time.monotonic() - started, 2),
            final_context=final, resumed_schedule=schedule, parent_audit=audit(parent['id']), child_audit=audit(child['id']),
            evidence_scope='Ten-second local fixture heartbeat recovery only; no production business-health claim')
        print('Independent recovery passed. Continuous parent remains active and armed.', flush=True)
    except Exception as error:
        report.update(status='failed', error=str(error))
        raise
    finally:
        producer_stop.set()
        producer.join(timeout=10)
        if parent and daemon.poll() is None:
            try:
                current = call({'op': 'get_loop', 'id': parent['id']})
                report['paused_parent'] = current if current['state'] == 'paused' else control(parent['id'], 'pause')
            except Exception as error:
                report['pause_error'] = str(error)
        if daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=30)
            except subprocess.TimeoutExpired:
                report['cleanup_error'] = 'Isolated daemon remains live; inspect retained PID'
        report['daemon_exit_code'] = daemon.poll()
        if producer.is_alive() or report.get('pause_error') or report.get('cleanup_error') or daemon.poll() != 0:
            report['status'] = 'cleanup_failed'
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        if report['status'] == 'cleanup_failed':
            raise RuntimeError('Pilot cleanup incomplete; inspect report')


if __name__ == '__main__':
    main()
