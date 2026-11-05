#!/usr/bin/env python3
"""Actual collector process signals/ownership against an intentionally offline state."""
import argparse
import json
from pathlib import Path
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    output = parser.parse_args().output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    state = output / 'offline-state'
    state.mkdir()
    binary = ROOT / 'target/debug/telos'
    report = {'status': 'running', 'scope': 'native collector lifecycle, intentionally offline source', 'signals': {}}
    try:
        for name, stop_signal in [('SIGINT', signal.SIGINT), ('SIGTERM', signal.SIGTERM)]:
            log = output / (name + '.jsonl')
            base = [str(binary), '--state-dir', str(state), 'observe-service',
                    '--output', str(log), '--timeout-seconds', '1']
            process = subprocess.Popen(base + ['--samples', '100', '--interval-seconds', '60'],
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    assert process.poll() is None, 'collector exited early'
                    if log.exists() and log.read_bytes().endswith(b'\n'):
                        break
                    time.sleep(.05)
                else:
                    raise AssertionError('no first observation')
                before = log.read_bytes()
                duplicate = subprocess.run(base, capture_output=True, text=True, timeout=5)
                assert duplicate.returncode != 0 and 'another collector' in duplicate.stderr
                assert log.read_bytes() == before
                process.send_signal(stop_signal)
                stdout, stderr = process.communicate(timeout=10)
                assert process.returncode == 0, stderr
                stopped = json.loads(stdout)
                assert stopped['interrupted'] and stopped['recorded_samples'] == 1
                assert stopped['error_samples'] == 1 and stopped['healthy_samples'] == 0
                assert log.read_bytes() == before
                # The persistent lock file is not ownership: the kernel lock is released.
                resumed = subprocess.run(base, capture_output=True, text=True, timeout=5)
                assert resumed.returncode == 0, resumed.stderr
                assert json.loads(resumed.stdout)['healthy'] is False
                assert len(log.read_text().splitlines()) == 2
                report['signals'][name] = {'graceful_exit': True, 'duplicate_rejected': True,
                                           'no_shutdown_heartbeat': True, 'lock_released': True}
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.communicate(timeout=10)
        report['status'] = 'passed'
    except Exception as error:
        report.update(status='failed', error=str(error))
        raise
    finally:
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
