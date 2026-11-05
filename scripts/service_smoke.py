#!/usr/bin/env python3
"""Actual user launchd lifecycle for an isolated installation, no model or credentials."""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    home = output / 'codex-home'
    home.mkdir()
    prefix = output / 'install'
    state = output / 'state'
    binary = ROOT / 'target/debug/telos'
    codex = shutil.which('codex')
    assert codex
    report = {'status': 'running', 'surface': 'actual_macos_launchd',
              'scope': 'isolated empty installation; no model, remote project, or login/reboot test'}

    def command(*args, expect=True):
        result = subprocess.run([str(binary), *map(str, args)], capture_output=True, text=True, timeout=50)
        if expect:
            assert result.returncode == 0, result.stderr
            return json.loads(result.stdout)
        return result

    try:
        command('--state-dir', state, 'install', '--prefix', prefix, '--codex-home', home, '--codex', codex)
        running = command('service', 'install', '--prefix', prefix, '--codex', codex)
        assert running['healthy']
        pid = running['daemon']['pid']
        assert pid == running['launchd']['pid']
        duplicate = command('service', 'install', '--prefix', prefix, '--codex', codex)
        assert duplicate['healthy'] and duplicate['daemon']['pid'] == pid
        for action in ['upgrade', 'uninstall']:
            denied = command(action, '--prefix', prefix, '--codex-home', home, '--codex', codex, expect=False)
            assert denied.returncode and 'remove launchd service' in denied.stderr
        plist = Path(running['plist'])
        original = plist.read_bytes()
        plist.write_bytes(original + b'\n')
        try:
            denied = command('service', 'remove', '--prefix', prefix, expect=False)
            assert denied.returncode and 'modified' in denied.stderr
            assert plist.read_bytes() == original + b'\n'
        finally:
            plist.write_bytes(original)
        current = command('service', 'status', '--prefix', prefix)
        assert current['healthy'] and current['daemon']['pid'] == pid
        expected = f'{prefix}/telos --state-dir {state} daemon --codex {Path(codex).resolve()}'
        actual = subprocess.run(['ps', '-p', str(pid), '-o', 'command='], check=True, capture_output=True, text=True).stdout.strip()
        assert actual == expected, actual
        os.kill(pid, signal.SIGKILL)
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            recovered = command('service', 'status', '--prefix', prefix)
            if recovered['healthy'] and recovered['daemon']['pid'] != pid:
                break
            time.sleep(.5)
        else:
            raise RuntimeError('launchd did not recover the terminated fixture process')
        removed = command('service', 'remove', '--prefix', prefix)
        assert removed['removed'] and not plist.exists()
        assert (state / 'telos.db').exists() and (state / 'service-logs').is_dir()
        assert command('service', 'status', '--prefix', prefix)['configured'] is False
        report.update(status='passed', checks={'installed_and_pid_matched': True,
                      'duplicate_install_kept_same_process': True, 'modified_plist_preserved': True,
                      'upgrade_and_uninstall_blocked_while_managed': True,
                      'crash_restarted_with_new_pid': True, 'bootout_confirmed': True,
                      'state_and_logs_preserved': True}, target=running['target'])
    except Exception as error:
        report.update(status='failed', error=str(error))
        if (prefix / 'service.json').exists():
            status = command('service', 'status', '--prefix', prefix, expect=False)
            report['last_service_status'] = status.stdout or status.stderr
            saved = json.loads((prefix / 'service.json').read_text())
            detail = subprocess.run(['/bin/launchctl', 'print', saved['target']], capture_output=True, text=True, timeout=10)
            (output / 'launchd-detail.txt').write_text(detail.stdout + detail.stderr)
        raise
    finally:
        if (prefix / 'service.json').exists():
            cleanup = command('service', 'remove', '--prefix', prefix, expect=False)
            report['cleanup_exit_code'] = cleanup.returncode
            if cleanup.returncode:
                report['cleanup_error'] = cleanup.stderr
        if (prefix / 'install.json').exists() and not (prefix / 'service.json').exists():
            cleanup = command('uninstall', '--prefix', prefix, '--codex-home', home, '--codex', codex, expect=False)
            report['bundle_cleanup_exit_code'] = cleanup.returncode
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
