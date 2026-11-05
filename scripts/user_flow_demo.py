#!/usr/bin/env python3
"""Run a real installed Telos delivery for a new language-neutral demo project.

The user speaks a requirement in Codex. This frontend harness creates the local
project/package and selects independent tests; installed Telos owns the actual
Codex work and acceptance. Never resubmit a known goal to retrieve progress.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
TESTS = r'''import json, pathlib, subprocess, sys, tempfile, unittest
NODE=sys.argv.pop(1)
CHECKER=pathlib.Path.cwd()/'check-links.mjs'
class LinkChecks(unittest.TestCase):
    def setUp(self):
        scratch=pathlib.Path.cwd()/'.test-output'
        scratch.mkdir(exist_ok=True)
        self.temp=tempfile.TemporaryDirectory(prefix='md links ',dir=scratch)
        self.root=pathlib.Path(self.temp.name)
    def tearDown(self): self.temp.cleanup()
    def run_checker(self,*args):
        return subprocess.run([NODE,str(CHECKER),*map(str,args)],capture_output=True,text=True,timeout=10)
    def document(self,text):
        p=self.root/'README.md';p.write_text(text);return p
    def result(self,path):
        p=self.run_checker(path)
        return p,json.loads(p.stdout)
    def test_existing_local_chinese_space_and_fragment(self):
        (self.root/'intro.md').write_text('# Part')
        (self.root/'中文 文件.md').write_text('ok')
        p,v=self.result(self.document('[intro](intro.md#part)\n[中文](<中文 文件.md>)'))
        self.assertEqual(p.returncode,0,p.stderr)
        self.assertEqual(v['checked'],2);self.assertEqual(v['broken'],[])
    def test_broken_link_is_nonzero_and_reports_target(self):
        p,v=self.result(self.document('[bad](missing.md)'))
        self.assertEqual(p.returncode,1,p.stderr)
        self.assertEqual(v['checked'],1);self.assertEqual(len(v['broken']),1)
        self.assertEqual(v['broken'][0]['target'],'missing.md')
    def test_external_mail_and_anchor_are_ignored(self):
        p,v=self.result(self.document('[web](https://example.invalid/x)\n[mail](mailto:a@example.invalid)\n[part](#section)'))
        self.assertEqual(p.returncode,0,p.stderr)
        self.assertEqual(v['checked'],0);self.assertEqual(v['ignored'],3);self.assertEqual(v['broken'],[])
    def test_relative_paths_use_markdown_parent_not_process_cwd(self):
        (self.root/'file.md').write_text('ok')
        docs=self.root/'nested';docs.mkdir()
        source=docs/'doc.md';source.write_text('[up](../file.md)')
        p,v=self.result(source)
        self.assertEqual(p.returncode,0,p.stderr);self.assertEqual(v['checked'],1);self.assertEqual(v['broken'],[])
    def test_unreadable_input_is_usage_error(self):
        p=self.run_checker(self.root/'not-there.md')
        self.assertEqual(p.returncode,2);self.assertTrue(p.stderr.strip())
    def test_missing_arguments_is_usage_error(self):
        p=self.run_checker()
        self.assertEqual(p.returncode,2);self.assertTrue(p.stderr.strip())
if __name__=='__main__': unittest.main(verbosity=2)
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--adapter', choices=['local.sandbox_command', 'local.sandbox_json'], default='local.sandbox_command')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    codex = shutil.which('codex')
    assert codex
    registration = json.loads(subprocess.check_output([codex, 'mcp', 'get', 'telos', '--json'], text=True))
    transport = registration['transport']
    assert transport['type'] == 'stdio'
    binary = Path(transport['command'])
    state_args = transport['args']
    state = Path(state_args[state_args.index('--state-dir') + 1])
    assert binary.name == 'telos' and binary.is_file() and state.is_dir()
    node = Path(shutil.which('node')).resolve()
    python = Path(sys.executable).resolve()
    project = output / 'project'
    project.mkdir()
    (project / '.gitignore').write_text('.test-output/\n')
    (project / 'README.md').write_text('# Markdown 本地链接检查器\n\n由 Telos 演示任务实现。\n')
    (project / 'AGENTS.md').write_text(
        '# Demo project\nImplement only check-links.mjs and README.md in your assigned worktree.\n'
        'Use Node.js built-in modules only; no dependencies, network, services, credentials or Git mutations.\n'
        'Do not edit AGENTS.md/.gitignore or access other projects. Do not run tests; independent frontend tests run automatically.\n'
        'When implementation is done return done plus changed paths; overall acceptance belongs to Telos.\n')
    for command in [('init', '--quiet'), ('add', '.'), ('commit', '-qm', 'Demo project baseline')]:
        subprocess.run(['git', '-C', str(project), '-c', 'user.name=Telos Demo', '-c',
                        'user.email=demo@example.invalid', '-c', 'commit.gpgSign=false',
                        '-c', 'core.hooksPath=/dev/null', *command], capture_output=True, check=True)
    acceptance = output / 'acceptance'
    acceptance.mkdir()
    tests = acceptance / 'test_links.py'
    tests.write_text(TESTS)
    if args.adapter == 'local.sandbox_json':
        wrapper = acceptance / 'verify.py'
        wrapper.write_text('import json, subprocess, sys\n'
            'result = subprocess.run(sys.argv[1:], capture_output=True, text=True)\n'
            'sys.stderr.write(result.stdout + result.stderr)\n'
            'print(json.dumps({"criterion_id":"link-checker", "verdict":"passed" if result.returncode == 0 else "failed", "summary":"Six independent CLI behavior tests; details in stderr"}))\n')
    project_id = 'demo-linkcheck-'  + uuid.uuid4().hex[:8]
    report = {'status': 'preparing', 'project_id': project_id, 'project_root': str(project),
              'binary': str(binary), 'state_dir': str(state),
              'scope': 'Actual installed daemon, real Codex implementation and independent native sandbox tests; isolated local JavaScript demo'}

    def save():
        (output / 'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')

    def call(request):
        path = output / 'operator-request.json'
        path.write_text(json.dumps(request))
        p = subprocess.run([str(binary), '--state-dir', str(state), 'request', str(path)],
                           capture_output=True, text=True, timeout=30)
        if p.returncode:
            raise RuntimeError(p.stderr)
        return json.loads(p.stdout)

    call({'op': 'register_project', 'id': project_id, 'root': str(project)})
    call({'op': 'configure_project', 'id': project_id, 'expected_revision': 0, 'policy': {
        'enabled': True, 'sandbox': 'workspace_write', 'timeout_seconds': 180,
        'max_output_bytes': 1048576, 'max_steps_per_cycle': 2}})
    package = {'project_id': project_id, 'name': '通用流程演示：Markdown 本地链接检查器', 'base': 'HEAD',
        'contract': {
            'goal': 'Implement a dependency-free Node.js CLI check-links.mjs. Usage: node check-links.mjs FILE.md. '
                    'Parse ordinary inline Markdown links [text](target), supporting angle-bracket targets with spaces/Chinese and local #fragments. '
                    'Resolve local relative file paths against the Markdown file parent, not cwd. Ignore http:, https:, mailto: and anchor-only links. '
                    'Print one JSON object with checked (number of local targets), ignored (number of ignored links) and broken '
                    '(array of objects with target, without angle brackets, at minimum). '
                    'Exit 0 when no local broken targets, exit 1 when broken targets exist, exit 2 with a clear stderr message for unreadable input or missing arguments. '
                    'Do not implement full CommonMark, nested parentheses or title syntax. Document usage and output in README.md.',
            'delivery_scope': 'Only check-links.mjs and README.md in assigned detached worktree. Node built-ins only. '
                    'Do not run tests/builds or install anything; independent acceptance follows automatically. '
                    'No network, credentials, external projects, services, Git history/index edits or changes to original checkout. '
                    'Preserve AGENTS.md and .gitignore. Return done after implementation, without claiming overall goal acceptance.',
            'criteria': [{'id': 'link-checker', 'description': 'Independent tests invoke actual delivered CLI with existing, broken, ignored, Chinese/space, fragment and nested relative-path cases and unreadable input; check JSON and exit codes.'}],
            'interval_seconds': None, 'max_attempts': 2},
        'verification': {'checks': [], 'log_checks': [], 'test_checks': [{
            'criterion_id': 'link-checker', 'adapter': args.adapter, 'command': {
                'program': str(python), 'args': ([str(wrapper), str(python), str(tests), str(node)] if args.adapter == 'local.sandbox_json' else [str(tests), str(node)]),
                'readable_roots': [str(Path(sys.prefix).resolve()), '/usr/local', '/bin', '/usr/bin', '/usr/lib',
                                   '/System/Library', '/Library/Developer/CommandLineTools', str(acceptance)],
                'timeout_seconds': 45, 'max_output_bytes': 65536}}]}}
    path = output / 'delivery.json'
    path.write_text(json.dumps(package, ensure_ascii=False, indent=2) + '\n')
    report.update(status='submitting', package=str(path), node=str(node), python=str(python), adapter=args.adapter)
    save()
    print('Submitting one requirement to the installed Telos delivery entry.', flush=True)
    started = time.monotonic()
    with (output / 'delivery-result.json').open('w') as result_file, (output / 'progress.log').open('w') as log:
        client = subprocess.Popen([str(binary), '--state-dir', str(state), 'deliver', '--file', str(path),
                                   '--wait', '--wait-seconds', '240'], stdout=result_file, stderr=subprocess.PIPE, text=True)
        for line in client.stderr:
            log.write(line)
            log.flush()
            print(line.rstrip(), flush=True)
            if line.startswith('Goal: '):
                report.update(loop_id=line.strip().split(' ', 1)[1], status='running')
                save()
        client.wait()
    if client.returncode:
        report.update(status='client_error', client_exit_code=client.returncode)
        save()
        raise RuntimeError('Delivery client ended with error; inspect known goal, do not resubmit')
    result = json.loads((output / 'delivery-result.json').read_text())
    report['first_follow'] = result
    deadline = time.monotonic() + 180
    while result['loop_state'] == 'active' and result['state'] != 'completed' and time.monotonic() < deadline:
        time.sleep(2)
        p = subprocess.run([str(binary), '--state-dir', str(state), 'follow', '--id', report['loop_id'], '--once'],
                           capture_output=True, text=True, timeout=20)
        if p.returncode:
            raise RuntimeError('Follow failed; retain this goal for inspection')
        result = json.loads(p.stdout)
        save()
    report.update(status='completed' if result['state'] == 'completed' else 'needs_inspection',
                  elapsed_seconds=round(time.monotonic() - started, 2), result=result)
    save()
    if report['status'] != 'completed':
        raise RuntimeError('Goal is not accepted; inspect its existing execution/evidence before continuing')
    assert result['delivery']['verdict'] == 'passed'
    exported = subprocess.run([str(binary), '--state-dir', str(state), 'review', '--id', report['loop_id'],
                               '--output', str(output / 'review')], capture_output=True, text=True, timeout=60)
    assert exported.returncode == 0, exported.stderr
    report['review_export'] = json.loads(exported.stdout)
    save()
    print('Installed Telos reports completed with independent acceptance and saved patch/evidence.', flush=True)


if __name__ == '__main__':
    main()
