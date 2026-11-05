#!/usr/bin/env python3
"""Real native-sandbox test acceptance through daemon; fixture edit is by harness."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    project = output / 'project'
    project.mkdir()
    binary = ROOT / 'target/debug/telos'
    codex = shutil.which('codex')
    assert codex
    state = output / 'state'
    report = {'status': 'running', 'executor': 'actual_codex_native_sandbox',
              'scope': 'synthetic project tests; harness edits fixture, no model or automatic repair',
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}

    def git(*args):
        subprocess.run(['git', '-C', str(project), '-c', 'user.name=Telos Fixture',
                        '-c', 'user.email=fixture@example.invalid', '-c', 'commit.gpgSign=false',
                        '-c', 'core.hooksPath=/dev/null', *args], check=True, capture_output=True, timeout=20)

    def call(request):
        path = output / 'request.json'
        path.write_text(json.dumps(request))
        result = subprocess.run([str(binary), '--state-dir', str(state), 'request', str(path)],
                                capture_output=True, text=True, timeout=45)
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)

    git('init', '--quiet')
    (project / 'value.txt').write_text('before\n')
    git('add', '.')
    git('commit', '-qm', 'fixture')
    with (output / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([str(binary), '--state-dir', str(state), 'daemon', '--codex', codex], stdout=log, stderr=log)
    try:
        for _ in range(100):
            try:
                call({'op': 'ping'})
                break
            except (AssertionError, FileNotFoundError):
                assert daemon.poll() is None
                time.sleep(.1)
        else:
            raise RuntimeError('daemon startup timeout')
        call({'op': 'register_project', 'id': 'fixture', 'root': str(project)})
        call({'op': 'configure_project', 'id': 'fixture', 'expected_revision': 0, 'policy': {
            'enabled': True, 'sandbox': 'workspace_write', 'timeout_seconds': 30,
            'max_output_bytes': 65536, 'max_steps_per_cycle': 3}})
        goal = call({'op': 'create_loop', 'project_id': 'fixture', 'name': 'Independent project tests',
                     'kind': 'delivery', 'contract': {
                         'goal': 'Fixture value equals after', 'delivery_scope': 'value.txt',
                         'criteria': [{'id': 'value', 'description': 'Independent Python assertion passes'}],
                         'interval_seconds': None, 'max_attempts': 3}})
        call({'op': 'configure_verification', 'id': goal['id'], 'revision': 1, 'expected_version': 0,
              'plan': {'test_checks': [{'criterion_id': 'value', 'command': {
                  'program': str(Path(sys.executable).resolve()),
                  'args': ['-c', "from pathlib import Path; value=Path('value.txt').read_text(); print(repr(value)); assert value=='after\\n'"],
                  'readable_roots': [str(Path(sys.prefix).resolve())],
                  'timeout_seconds': 15, 'max_output_bytes': 65536}}]}})
        workspace = call({'op': 'prepare_workspace', 'id': goal['id'], 'revision': 1, 'policy_revision': 1, 'base': 'HEAD'})
        verify = {'op': 'verify_loop', 'id': goal['id'], 'revision': 1}
        first = call(verify)
        assert first['verdict'] == 'failed', first
        assert call({'op': 'get_loop', 'id': goal['id']})['state'] == 'active'
        (Path(workspace['path']) / 'value.txt').write_text('after\n')
        second = call(verify)
        assert second['verdict'] == 'passed', second
        assert call({'op': 'get_loop', 'id': goal['id']})['state'] == 'completed'
        assert (project / 'value.txt').read_text() == 'before\n'
        evidence = call({'op': 'get_evidence', 'id': second['evidence_id'], 'offset': 0, 'limit': 32768})
        body = json.loads(evidence['text'])
        observation = body['observations'][0]['detail']['report']
        assert observation['exit_code'] == 0 and observation['cleanup_confirmed']
        delivery = call({'op': 'delivery_result', 'id': goal['id'], 'revision': 1})
        assert delivery['verdict'] == 'passed' and delivery['review_evidence_id']
        saved = call({'op': 'list_evidence', 'id': goal['id'], 'limit': 100})
        contents = [call({'op': 'get_evidence', 'id': e['id'], 'offset': 0, 'limit': 32768})['text'] for e in saved['items']]
        assert "'after\\n'\n" in contents
        report.update(status='passed', checks={'failing_tests_keep_goal_active': True,
                      'passing_tests_complete_goal': True, 'original_checkout_preserved': True,
                      'raw_test_output_saved_as_evidence': True, 'review_patch_saved': True,
                      'native_sandbox_test_cleanup_confirmed': True}, loop_id=goal['id'])
    except Exception as error:
        report.update(status='failed', error=str(error))
        raise
    finally:
        daemon.terminate()
        daemon.wait(timeout=30)
        report['daemon_exit_code'] = daemon.returncode
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
