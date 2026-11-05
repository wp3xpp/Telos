#!/usr/bin/env python3
"""Synthetic first attempt, real Codex automatic correction and native test acceptance."""
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
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/telos')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    project = output / 'project'
    project.mkdir()
    binary = args.binary.resolve()
    codex = shutil.which('codex')
    assert codex
    state = output / 'state'
    report = {'status': 'running', 'executor': 'actual_codex_repair_and_native_sandbox',
              'scope': 'synthetic initial worker; real Codex automatic correction and native sandbox tests; not a real project pilot',
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
    wrapper = output / 'codex-fixture-bootstrap'
    wrapper.write_text("#!/usr/bin/env python3\nimport json,os,pathlib,subprocess,sys\nREAL=" + repr(codex) + "\n" + r"""
if sys.argv[1]=='sandbox':
    os.execv(REAL,[REAL,*sys.argv[1:]])
prompt=sys.stdin.read()
if 'Repair the independently verified failure' in prompt:
    sys.exit(subprocess.run([REAL,*sys.argv[1:]],input=prompt,text=True).returncode)
# Deterministic first incomplete attempt, used only to force the correction path.
args=sys.argv
result={'status':'done','summary':'Synthetic initial attempt; independent acceptance required','evidence_paths':[],'remaining_work':[]}
pathlib.Path(args[args.index('-o')+1]).write_text(json.dumps(result))
print(json.dumps({'type':'turn.completed','usage':{'output_tokens':0}}))
""")
    wrapper.chmod(0o700)
    with (output / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([str(binary), '--state-dir', str(state), 'daemon', '--codex', str(wrapper)], stdout=log, stderr=log)
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
            'enabled': True, 'sandbox': 'workspace_write', 'timeout_seconds': 120,
            'max_output_bytes': 1048576, 'max_steps_per_cycle': 3}})
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
        call({'op': 'start_loop', 'id': goal['id'], 'revision': 1, 'request_key': 'bootstrap-once'})
        follow_output = (output / 'follow.json').open('w')
        follow_log = (output / 'follow.log').open('w')
        waiter = subprocess.Popen([str(binary), '--state-dir', str(state), 'follow', '--id', goal['id'],
                                   '--wait-seconds', '180'], stdout=follow_output, stderr=follow_log)
        deadline = time.monotonic() + 180
        observed_failure = False
        while time.monotonic() < deadline:
            current = call({'op': 'get_loop', 'id': goal['id']})
            delivery = call({'op': 'delivery_result', 'id': goal['id'], 'revision': 1})
            observed_failure |= delivery.get('verdict') == 'failed'
            if current['state'] == 'completed':
                break
            time.sleep(.5)
        else:
            raise RuntimeError('real correction did not complete within probe deadline')
        waiter.wait(timeout=20)
        follow_output.close()
        follow_log.close()
        assert waiter.returncode == 0
        followed = json.loads((output / 'follow.json').read_text())
        assert followed['state'] == 'completed', 'follow returned before bounded background correction completed'
        assert 'correcting' in (output / 'follow.log').read_text()
        report['follow'] = followed
        assert observed_failure
        context = call({'op': 'loop_context', 'id': goal['id'], 'revision': 1})
        assert len(context['recent_execution']) >= 2
        second = {'evidence_id': context['latest_verification']['results'][0]['evidence_ids'][0]}
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
        report.update(status='passed', checks={'synthetic_initial_attempt': True, 'failure_observed_before_repair': True, 'real_codex_corrected_worktree': True,
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
