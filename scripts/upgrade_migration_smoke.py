#!/usr/bin/env python3
"""Rehearse a real older binary -> current bundle/database upgrade in isolation."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import sqlite3
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--old-binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    old = args.old_binary.resolve()
    new = ROOT / 'target/debug/telos'
    home, prefix, state = (output / name for name in ('codex-home', 'install', 'state'))
    home.mkdir()
    project = output / 'project'
    project.mkdir()
    codex = shutil.which('codex')
    assert codex
    daemon = None
    report = {'status': 'running', 'scope': 'isolated real binary upgrade; synthetic paused goal; no model or live state',
              'old_binary_sha256': hashlib.sha256(old.read_bytes()).hexdigest(),
              'new_binary_sha256': hashlib.sha256(new.read_bytes()).hexdigest()}

    def run(binary, *arguments):
        result = subprocess.run([str(binary), *map(str, arguments)], capture_output=True, text=True, timeout=60)
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)

    def call(request):
        path = output / 'request.json'
        path.write_text(json.dumps(request))
        return run(new, '--state-dir', state, 'request', path)

    def start(binary, name):
        nonlocal daemon
        with (output / (name + '.log')).open('w') as log:
            daemon = subprocess.Popen([str(binary), '--state-dir', str(state), 'daemon',
                                       '--codex', str(output / 'nonexistent-worker')], stdout=log, stderr=log)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            assert daemon.poll() is None, (output / (name + '.log')).read_text()
            try:
                call({'op': 'ping'})
                return
            except AssertionError:
                time.sleep(.05)
        raise AssertionError('startup timeout')

    def stop():
        nonlocal daemon
        if daemon is not None:
            daemon.terminate()
            daemon.wait(timeout=15)
            assert daemon.returncode == 0
            daemon = None

    def schema():
        with sqlite3.connect((state / 'telos.db').as_uri() + '?mode=ro', uri=True) as db:
            return db.execute('SELECT MAX(version) FROM schema_migrations').fetchone()[0]

    try:
        run(old, '--state-dir', state, 'install', '--prefix', prefix, '--codex-home', home, '--codex', codex)
        start(prefix / 'telos', 'old-daemon')
        call({'op': 'register_project', 'id': 'fixture', 'root': str(project)})
        goal = call({'op': 'create_loop', 'project_id': 'fixture', 'name': 'upgrade fixture', 'kind': 'delivery',
                     'contract': {'goal': 'Preserve this contract during upgrade', 'delivery_scope': 'synthetic only',
                                  'criteria': [{'id': 'a', 'description': 'fixture'}],
                                  'max_attempts': 2, 'interval_seconds': None}})
        call({'op': 'control', 'id': goal['id'], 'expected_version': goal['version'], 'action': 'pause'})
        before = call({'op': 'get_loop', 'id': goal['id']})
        stop()
        report['schema_before'] = schema()
        snapshot = output / 'pre-upgrade-backup'
        run(new, '--state-dir', state, 'backup', 'create', '--destination', snapshot)
        run(new, 'backup', 'verify', '--backup', snapshot)
        backup_hash = hashlib.sha256((snapshot / 'state/telos.db').read_bytes()).hexdigest()
        registration = (home / 'config.toml').read_bytes()
        upgraded = run(new, 'upgrade', '--prefix', prefix, '--codex-home', home, '--codex', codex)
        assert upgraded['upgraded'] and not upgraded['database_migrated']
        assert schema() == report['schema_before']
        assert (home / 'config.toml').read_bytes() == registration
        assert (prefix / 'telos').read_bytes() == new.read_bytes()
        start(prefix / 'telos', 'new-daemon')
        assert call({'op': 'get_loop', 'id': goal['id']}) == before
        stop()
        report['schema_after'] = schema()
        assert report['schema_before'] < report['schema_after'] == 19
        # The old executable must refuse a migrated DB rather than silently downgrade.
        rejected = subprocess.run([str(old), '--state-dir', str(state), 'daemon'],
                                  capture_output=True, text=True, timeout=15)
        assert rejected.returncode != 0 and 'newer' in rejected.stderr
        assert hashlib.sha256((snapshot / 'state/telos.db').read_bytes()).hexdigest() == backup_hash
        run(new, 'backup', 'verify', '--backup', snapshot)
        report.update(status='passed', checks={'bundle_replaced': True, 'registration_preserved': True,
                      'paused_goal_preserved': True, 'migration_on_startup_only': True,
                      'old_binary_rejects_new_schema': True, 'pre_upgrade_backup_unchanged': True})
    except Exception as exc:
        report.update(status='failed', error=str(exc))
        raise
    finally:
        stop()
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
