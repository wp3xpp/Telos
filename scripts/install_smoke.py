#!/usr/bin/env python3
"""Exercise actual Codex config commands only in a fresh isolated CODEX_HOME."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    binary = ROOT / 'target/debug/telos'
    codex = shutil.which('codex')
    assert codex
    home = output / 'codex-home'
    home.mkdir()
    (home / 'config.toml').write_text('model = "fixture-model"\n[mcp_servers.unrelated]\ncommand = "fixture-other-command"\n')
    prefix = output / 'installed'
    state = output / 'state'
    report = {'status': 'running', 'checks': {}}

    def run(action, program=codex, target=prefix, ok=True):
        result = subprocess.run([str(binary), '--state-dir', str(state), action, '--prefix', str(target),
                                 '--codex-home', str(home), '--codex', str(program)],
                                capture_output=True, text=True, timeout=60)
        assert (result.returncode == 0) == ok, {'action': action, 'stdout': result.stdout, 'stderr': result.stderr}
        return json.loads(result.stdout) if ok else result.stderr

    def mcp(name):
        result = subprocess.run([codex, 'mcp', 'get', name, '--json'],
                                env={**os.environ, 'CODEX_HOME': str(home)}, capture_output=True, text=True, timeout=20)
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)

    try:
        other = mcp('unrelated')
        installed = run('install')
        assert installed['daemon_started'] is False
        assert mcp('telos')['transport']['command'] == str(prefix / 'telos')
        assert mcp('unrelated') == other
        assert 'fixture-model' in (home / 'config.toml').read_text()
        skill = home / 'skills/hsw-telos/SKILL.md'
        original = skill.read_text()
        run('install', target=output / 'duplicate', ok=False)
        assert not (output / 'duplicate').exists()
        skill.write_text(original + '\nuser-edit\n')
        run('uninstall', ok=False)
        run('upgrade', ok=False)
        assert skill.read_text().endswith('user-edit\n')
        assert mcp('telos')['transport']['command'] == str(prefix / 'telos')
        skill.write_text(original)
        (state / 'retained.txt').write_text('retained fixture')
        # Ownership lock must reject upgrades/uninstall even with no listening daemon.
        with (state / 'daemon.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            run('upgrade', ok=False)
            run('uninstall', ok=False)
        # Represent an older owned bundle, then upgrade through real Codex inspection.
        (prefix / 'telos').write_bytes(b'old owned fixture executable')
        skill.write_text('old owned fixture skill')
        manifest = json.loads((prefix / 'install.json').read_text())
        manifest['binary_sha256'] = hashlib.sha256((prefix / 'telos').read_bytes()).hexdigest()
        manifest['skill_sha256'] = hashlib.sha256(skill.read_bytes()).hexdigest()
        (prefix / 'install.json').write_text(json.dumps(manifest, indent=2))
        registered = mcp('telos')
        upgraded = run('upgrade')
        assert upgraded['upgraded'] and not upgraded['daemon_started']
        assert (prefix / 'telos').read_bytes() == binary.read_bytes()
        assert skill.read_text() == original
        assert not (prefix / 'upgrade.json').exists()
        assert mcp('telos') == registered and mcp('unrelated') == other
        assert (state / 'retained.txt').read_text() == 'retained fixture'
        # Simulate interruption after the durable journal, before staging finishes.
        after = json.loads((prefix / 'install.json').read_text())
        (prefix / 'telos').write_bytes(b'old owned fixture executable')
        skill.write_text('old owned fixture skill')
        old_bytes = json.dumps(manifest, indent=2).encode()
        (prefix / 'install.json').write_bytes(old_bytes)
        journal = {'format': 1, 'prefix': str(prefix), 'before': manifest, 'after': after,
                   'before_manifest_sha256': hashlib.sha256(old_bytes).hexdigest()}
        (prefix / 'upgrade.json').write_text(json.dumps(journal))
        run('uninstall', ok=False)
        resumed = run('upgrade')
        assert resumed['resumed'] and (prefix / 'telos').read_bytes() == binary.read_bytes()
        assert skill.read_text() == original and not (prefix / 'upgrade.json').exists()
        run('uninstall')
        assert not prefix.exists() and not skill.exists()
        assert (state / 'retained.txt').read_text() == 'retained fixture'
        assert mcp('unrelated') == other
        # An MCP add failure must clean only this install's files.
        fake = output / 'failing-codex'
        fake.write_text("#!/usr/bin/env python3\nimport sys\nif 'get' in sys.argv:\n print(\"Error: No MCP server named 'telos' found.\",file=sys.stderr)\nelse:\n print('fixture registration failure',file=sys.stderr)\nsys.exit(1)\n")
        fake.chmod(0o700)
        run('install', program=fake, ok=False)
        assert not prefix.exists() and not skill.exists()
        assert mcp('unrelated') == other
        report.update(status='passed', checks={'actual_codex_registration': True, 'unrelated_config_preserved': True,
            'existing_install_rejected': True, 'modified_skill_preserved': True, 'uninstall_preserves_state': True,
            'failed_install_rolls_back': True, 'upgrade_replaces_owned_bundle': True,
            'offline_lock_required': True, 'interrupted_upgrade_resumes': True, 'upgrade_preserves_registration_and_state': True, 'desktop_discovery': 'not_verified'})
    except Exception as exc:
        report.update(status='failed', error=str(exc))
        raise
    finally:
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
