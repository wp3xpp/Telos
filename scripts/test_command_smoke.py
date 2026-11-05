#!/usr/bin/env python3
"""Actual native sandbox probe; no model, credentials or external services."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import socket
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    project = output / 'project'
    project.mkdir()
    outside = output / 'outside.txt'
    outside.write_text('synthetic control file\n')
    binary = ROOT / 'target/debug/telos'
    codex = shutil.which('codex')
    assert codex, 'Codex CLI required'
    checks = {}
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        listener.listen(1)
        port = listener.getsockname()[1]
        probe = '''import json,pathlib,socket,sys
checks={}
pathlib.Path('inside.txt').write_text('ok')
checks['workspace_write']=True
for mode in ['r','w']:
    try:
        with open(sys.argv[1],mode) as f:
            f.read() if mode=='r' else f.write('unexpected')
    except PermissionError:
        checks['outside_'+('read' if mode=='r' else 'write')+'_denied']=True
    else:
        raise AssertionError('outside access allowed')
with socket.socket() as sock:
    sock.settimeout(2)
    try:
        sock.connect(('127.0.0.1',int(sys.argv[2])))
    except PermissionError:
        checks['network_denied']=True
    else:
        raise AssertionError('network allowed')
print(json.dumps(checks))
'''
        for name, code, extra, expected in [
            ('boundaries', probe, [str(outside), str(port)], 'passed'),
            ('nonzero', 'import sys; print("intentional failure"); sys.exit(7)', [], 'failed'),
        ]:
            spec = {'program': str(Path(sys.executable).resolve()),
                    'args': ['-c', code, *extra],
                    'readable_roots': [str(Path(sys.prefix).resolve())],
                    'timeout_seconds': 20, 'max_output_bytes': 65536}
            request = output / (name + '.json')
            request.write_text(json.dumps(spec))
            run_dir = output / name
            completed = subprocess.run(
                [str(binary), 'test-command', '--request', str(request),
                 '--cwd', str(project), '--output', str(run_dir), '--codex', codex],
                capture_output=True, text=True, timeout=40)
            report = json.loads((run_dir / 'report.json').read_text())
            assert report['verdict'] == expected, report
            assert report['cleanup_confirmed'], report
            assert (completed.returncode == 0) == (expected == 'passed'), completed.stderr
            assert json.loads(completed.stdout) == report
            for stream in ['stdout', 'stderr']:
                digest = hashlib.sha256((run_dir / (stream + '.log')).read_bytes()).hexdigest()
                assert digest == report[stream + '_sha256']
            if name == 'boundaries':
                checks.update(json.loads((run_dir / 'stdout.log').read_text()))
            else:
                assert report['exit_code'] == 7, report
                checks['nonzero_exit_is_failed'] = True
    assert outside.read_text() == 'synthetic control file\n'
    assert (project / 'inside.txt').read_text() == 'ok'
    report = {'status': 'passed', 'executor': 'actual_codex_native_sandbox',
              'scope': 'local synthetic test commands; no model or goal acceptance',
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'checks': checks}
    (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
