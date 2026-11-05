#!/usr/bin/env python3
"""Real Codex edit in an isolated repository; hash acceptance, not a project test pilot."""
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
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    project = output / 'project'
    project.mkdir()
    codex = shutil.which('codex')
    assert codex
    binary = ROOT / 'target/debug/telos'
    state = output / 'state'
    report = {'status': 'running', 'executor': 'actual_codex', 'scope': 'isolated text-edit fixture; not project tests or real feature pilot'}

    def git(*args):
        subprocess.run(['git', '-C', str(project), '-c', 'user.name=Telos Fixture', '-c',
                        'user.email=fixture@example.invalid', '-c', 'commit.gpgSign=false', '-c',
                        'core.hooksPath=/dev/null', *args], check=True, capture_output=True, timeout=20)

    def call(request):
        path = output / 'request.json'
        path.write_text(json.dumps(request))
        result = subprocess.run([str(binary), '--state-dir', str(state), 'request', str(path)],
                                capture_output=True, text=True, check=True, timeout=45)
        return json.loads(result.stdout)

    git('init', '--quiet')
    (project / 'value.txt').write_bytes(b'before\n')
    (project / 'AGENTS.md').write_text('This is an isolated Telos fixture. Change only value.txt. No network access, dependency installation, Git commits or external commands are needed.\n')
    git('add', '.')
    git('commit', '-qm', 'fixture baseline')
    with (output / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([str(binary), '--state-dir', str(state), 'daemon', '--codex', codex], stdout=log, stderr=log)
    try:
        for _ in range(100):
            try:
                call({'op': 'ping'})
                break
            except (subprocess.CalledProcessError, FileNotFoundError):
                assert daemon.poll() is None
                time.sleep(0.1)
        else:
            raise RuntimeError('daemon startup timeout')
        call({'op': 'register_project', 'id': 'fixture', 'root': str(project)})
        call({'op': 'configure_project', 'id': 'fixture', 'expected_revision': 0, 'policy': {
            'enabled': True, 'sandbox': 'workspace_write', 'timeout_seconds': 120,
            'max_output_bytes': 1048576, 'max_steps_per_cycle': 1}})
        goal = call({'op': 'create_loop', 'project_id': 'fixture', 'name': 'Real isolated delivery probe', 'kind': 'delivery', 'contract': {
            'goal': 'Change value.txt to contain exactly after followed by a single newline.',
            'delivery_scope': 'Only value.txt in the assigned worktree. No commits or publishing.',
            'criteria': [{'id': 'value', 'description': 'value.txt has the independently specified bytes'}],
            'interval_seconds': None, 'max_attempts': 1}})
        call({'op': 'configure_verification', 'id': goal['id'], 'revision': 1, 'expected_version': 0,
              'plan': {'checks': [{'criterion_id': 'value', 'relative_path': 'value.txt',
                                   'expected_sha256': hashlib.sha256(b'after\n').hexdigest()}]}})
        workspace = call({'op': 'prepare_workspace', 'id': goal['id'], 'revision': 1, 'policy_revision': 1, 'base': 'HEAD'})
        step = call({'op': 'start_loop', 'id': goal['id'], 'revision': 1, 'request_key': 'real-delivery-probe'})
        deadline = time.monotonic() + 150
        while time.monotonic() < deadline:
            status = call({'op': 'step_status', 'id': step['step_id']})
            current = call({'op': 'get_loop', 'id': goal['id']})
            if current['state'] == 'completed':
                break
            if status['state'] in ('failed', 'outcome_unknown'):
                raise RuntimeError(f'worker ended with {status["state"]}')
            time.sleep(1)
        else:
            raise RuntimeError('delivery verification timeout')
        assert status['state'] == 'succeeded' and status['workspace_id'] == workspace['id']
        assert (project / 'value.txt').read_bytes() == b'before\n'
        assert (Path(workspace['path']) / 'value.txt').read_bytes() == b'after\n'
        delivery = call({'op': 'delivery_result', 'id': goal['id'], 'revision': 1})
        assert delivery['verdict'] == 'passed' and delivery['review_evidence_id']
        evidence = call({'op': 'get_evidence', 'id': delivery['review_evidence_id'], 'offset': 0, 'limit': 32768})
        review = json.loads(evidence['text'])
        assert '+after' in review['patch'] and delivery['artifact'].endswith(review['snapshot_sha256'])
        report.update(status='passed', checks={'actual_codex_write': True, 'original_checkout_preserved': True,
            'workspace_binding_recorded': True, 'independent_hash_acceptance': True, 'goal_completed_after_verification': True, 'review_patch_bound_to_accepted_snapshot': True},
            loop_id=goal['id'], step_id=step['step_id'])
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
