#!/usr/bin/env python3
"""Actual native sandbox + configured JSON verifier in an isolated local delivery.

No model worker, production endpoint, credential file access or desktop registration.
The fixture operator edits its assigned worktree only between completed checks.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--codex', default=shutil.which('codex'))
    args = parser.parse_args()
    assert args.codex
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    project = output / 'project'
    project.mkdir()
    (project / 'value').write_text('pending')
    for command in [('init', '--quiet'), ('add', '.'), ('commit', '-qm', 'fixture baseline')]:
        subprocess.run(['git', '-C', str(project), '-c', 'user.name=Fixture', '-c',
                        'user.email=fixture@example.invalid', '-c', 'commit.gpgSign=false',
                        '-c', 'core.hooksPath=/dev/null', *command], capture_output=True, check=True)
    state = Path(tempfile.mkdtemp(prefix='telos-json-check-', dir='/tmp')) / 'state'
    binary = ROOT / 'target/debug/telos'
    report = {'status': 'running', 'scope': 'isolated local delivery, real native sandbox, independent JSON assertion; no model worker',
              'state_dir': str(state), 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}

    def call(request, allow_error=False):
        path = output / 'request.json'
        path.write_text(json.dumps(request))
        result = subprocess.run([str(binary), '--state-dir', str(state), 'request', str(path)],
                                capture_output=True, text=True, timeout=60)
        if result.returncode:
            if allow_error:
                return {'rejected': True, 'error': result.stderr}
            raise RuntimeError(result.stderr)
        return json.loads(result.stdout)

    def observation(receipt):
        page = call({'op': 'get_evidence', 'id': receipt['evidence_id'], 'offset': 0, 'limit': 32768})
        assert page['integrity_verified'] and page['next_offset'] is None
        return json.loads(page['text'])

    with (output / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([str(binary), '--state-dir', str(state), 'daemon', '--codex', args.codex], stdout=log, stderr=log)
    report['daemon_pid'] = daemon.pid
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
            raise RuntimeError('isolated daemon did not start')
        call({'op': 'register_project', 'id': 'generic-json', 'root': str(project)})
        call({'op': 'configure_project', 'id': 'generic-json', 'expected_revision': 0, 'policy': {
            'enabled': True, 'sandbox': 'workspace_write', 'timeout_seconds': 30,
            'max_output_bytes': 65536, 'max_steps_per_cycle': 1}})
        goal = call({'op': 'create_loop', 'project_id': 'generic-json', 'kind': 'delivery',
            'name': 'Generic structured verifier native pilot', 'contract': {
                'goal': 'Independently verify the fixture value is approved.', 'delivery_scope': 'Local fixture only.',
                'criteria': [{'id': 'value', 'description': 'Configured independent JSON assertion confirms actual value is approved'}],
                'interval_seconds': None, 'max_attempts': 2}})
        report['loop_id'] = goal['id']
        command = {'program': '/bin/sh', 'args': ['-c',
            'if test "$(cat value)" = approved; then printf \'{"criterion_id":"value","verdict":"passed","summary":"actual value approved"}\'; '
            'else printf \'{"criterion_id":"value","verdict":"failed","summary":"actual value not approved"}\'; fi'],
            'readable_roots': ['/bin', '/usr/bin', '/usr/lib', '/System/Library'],
            'timeout_seconds': 20, 'max_output_bytes': 65536}
        request = {'op': 'configure_verification', 'id': goal['id'], 'revision': 1, 'expected_version': 0,
            'plan': {'test_checks': [{'criterion_id': 'value', 'adapter': 'not.registered', 'command': command}]}}
        rejection = call(request, allow_error=True)
        assert rejection['rejected'] and 'not registered' in rejection['error']
        request['plan']['test_checks'][0]['adapter'] = 'local.sandbox_json'
        plan = call(request)
        assert plan['version'] == 1, 'rejected adapter must not have consumed a plan version'
        workspace = call({'op': 'prepare_workspace', 'id': goal['id'], 'revision': 1, 'policy_revision': 1, 'base': 'HEAD'})
        failed = call({'op': 'verify_loop', 'id': goal['id'], 'revision': 1})
        assert failed['verdict'] == 'failed'
        observed_failed = observation(failed)
        detail = observed_failed['observations'][0]['detail']
        assert detail['adapter'] == 'local.sandbox_json'
        assert detail['report']['exit_code'] == 0 and detail['report']['cleanup_confirmed']
        assert detail['assertion']['verdict'] == 'failed'
        assert call({'op': 'get_loop', 'id': goal['id']})['state'] == 'active'
        print('Real sandbox command exited zero; independent failed assertion kept the goal active.', flush=True)
        # Fixture operator handoff, not an agent edit or concurrent worker mutation.
        (Path(workspace['path']) / 'value').write_text('approved')
        passed = call({'op': 'verify_loop', 'id': goal['id'], 'revision': 1})
        assert passed['verdict'] == 'passed'
        observed_passed = observation(passed)
        assert observed_passed['observations'][0]['detail']['assertion']['criterion_id'] == 'value'
        assert observed_passed['observations'][0]['detail']['report']['cleanup_confirmed']
        assert call({'op': 'get_loop', 'id': goal['id']})['state'] == 'completed'
        assert (project / 'value').read_text() == 'pending'
        delivery = call({'op': 'delivery_result', 'id': goal['id'], 'revision': 1})
        assert delivery['verdict'] == 'passed' and delivery['review_evidence_id']
        report.update(status='passed', elapsed_seconds=round(time.monotonic() - started, 2),
            adapter='local.sandbox_json', unknown_adapter_rejection=rejection, plan=plan,
            workspace=workspace, failed=failed, failed_observation=observed_failed,
            passed=passed, passed_observation=observed_passed, delivery=delivery,
            no_project_scripts_required=not (project / 'scripts').exists(), no_model_worker_dispatched=True)
        print('Matching structured assertion accepted; goal completed with artifact-bound evidence.', flush=True)
    except Exception as error:
        report.update(status='failed', error=str(error))
        raise
    finally:
        if daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=30)
            except subprocess.TimeoutExpired:
                report['cleanup_error'] = 'Isolated daemon remains live; inspect recorded PID before retrying'
        report['daemon_exit_code'] = daemon.poll()
        if daemon.poll() != 0:
            report['status'] = 'cleanup_failed'
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        if report['status'] == 'cleanup_failed':
            raise RuntimeError('Isolated pilot cleanup incomplete; inspect retained report')


if __name__ == '__main__':
    main()
