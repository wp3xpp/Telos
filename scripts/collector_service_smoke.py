#!/usr/bin/env python3
"""Real launchd collector lifecycle with an isolated deliberately offline source."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', required=True, type=Path)
    output = parser.parse_args().output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    state = output / 'offline-state'
    state.mkdir()
    directory = output / 'collector'
    binary = ROOT / 'target/debug/telos'
    report = {'status': 'running', 'scope': 'actual launchd collector; isolated offline source, no business or long-duration acceptance'}

    def command(action, *extra, expect=True):
        result = subprocess.run([str(binary), '--state-dir', str(state), 'collector', action,
                                 '--directory', str(directory), *extra], capture_output=True, text=True, timeout=50)
        assert (result.returncode == 0) == expect, result.stderr
        return json.loads(result.stdout) if expect else result.stderr

    try:
        first = command('install', '--interval-seconds', '1', '--timeout-seconds', '1')
        assert first['running'] and first['fresh_observation']
        assert first['observation']['kind'] == 'error'
        pid = first['launchd']['pid']
        assert command('install', '--interval-seconds', '1', '--timeout-seconds', '1')['launchd']['pid'] == pid
        command('install', '--interval-seconds', '2', '--timeout-seconds', '1', expect=False)
        plist = Path(first['plist'])
        original = plist.read_bytes()
        try:
            plist.write_bytes(original + b'\n')
            assert 'modified' in command('remove', expect=False)
        finally:
            plist.write_bytes(original)
        current = command('status')
        pid = current['launchd']['pid']
        identity = subprocess.check_output(['ps', '-p', str(pid), '-o', 'command='], text=True).strip()
        assert identity.startswith(str(directory / 'telos') + ' --state-dir ' + str(state) + ' observe-service '), identity
        os.kill(pid, signal.SIGKILL)
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            recovered = command('status')
            if recovered['running'] and recovered['launchd']['pid'] != pid and recovered['fresh_observation']:
                break
            time.sleep(.25)
        else:
            raise AssertionError('collector not restarted')
        assert recovered['observation']['kind'] == 'error'
        removed = command('remove')
        assert removed['removed'] and removed['observations_preserved']
        assert not plist.exists()
        assert command('status')['running'] is False
        assert (directory / 'observations.jsonl').stat().st_size > 0
        # Stop preserves a valid owned bundle and permits an explicit reinstall.
        again = command('install', '--interval-seconds', '1', '--timeout-seconds', '1')
        assert again['running'] and again['fresh_observation']
        command('remove')
        report.update(status='passed', checks={'offline_source_recorded_as_error': True,
                      'duplicate_install_idempotent': True, 'changed_settings_rejected': True,
                      'modified_plist_preserved': True, 'crashed_collector_restarted': True,
                      'remove_preserves_records': True, 'explicit_reinstall': True})
    except Exception as error:
        report.update(status='failed', error=str(error))
        raise
    finally:
        try:
            if (directory / 'collector.json').exists():
                command('remove')
        finally:
            (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
