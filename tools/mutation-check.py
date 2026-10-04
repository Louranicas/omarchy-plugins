#!/usr/bin/env python3
"""Selected semantic mutation controls; not a whole-project mutation score."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'target' / 'mutation-evidence'
CASES = [
    ('vimarchy-reply-fence', 'vimarchy-runtime/src/presenter.rs',
     '|| reply.auth != self.auth', '|| false',
     'vimarchy-runtime', 'presenter_link', 'reply_fence_revision_pagination_and_geometry_fail_closed'),
    ('ask-runtime-instance', 'ask-runtime/src/lib.rs',
     'if instance != self.instance {', 'if false {',
     'ask-runtime', 'protocol', 'permission_callback_from_old_runtime_cannot_approve_new_runtime'),
    ('yoohoo-revision-floor', 'yoohoo-runtime/src/presenter.rs',
     'minimum.is_some_and(|r| reply.revision.get() < r.get())', 'false',
     'yoohoo-runtime', 'presenter_refresh', 'fresh_model_cannot_roll_back_previous_revision'),
]


def run(args, cwd, env, log):
    # Kill the entire owned process group on timeout, including fixture children.
    with log.open('w') as stream:
        child = subprocess.Popen(args, cwd=cwd, env=env, stdout=stream,
                                 stderr=subprocess.STDOUT, start_new_session=True)
        try:
            return child.wait(timeout=300)
        except BaseException:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
            raise


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    results = []
    receipt = {'scope': 'Three selected semantic controls, not coverage or a mutation score',
               'passed': False, 'cases': results}
    (OUT / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    with tempfile.TemporaryDirectory(prefix='omarchy-mutations-') as directory:
        copy = Path(directory) / 'source'
        copy.mkdir()
        # Do not copy untracked credentials, build outputs or workstation evidence.
        tracked = subprocess.check_output(['git', 'ls-files', '-z'], cwd=ROOT)
        for raw in tracked.split(b'\0'):
            if not raw:
                continue
            relative = Path(os.fsdecode(raw))
            destination = copy / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / relative, destination, follow_symlinks=False)
        env = dict(os.environ, CARGO_TARGET_DIR=str(Path(directory) / 'build'),
                   CARGO_BUILD_JOBS='2', CARGO_PROFILE_DEV_DEBUG='0')
        for name, relative, needle, replacement, package, target, test in CASES:
            source = copy / relative
            original = source.read_text()
            if original.count(needle) != 1:
                raise RuntimeError(f'{name}: mutation anchor changed; review recipe')
            args = ['cargo', 'test', '--locked', '--offline', '-p', package,
                    '--test', target, test]
            code = run(args + ['--', '--exact'], copy, env, OUT / f'{name}-baseline.log')
            baseline = (OUT / f'{name}-baseline.log').read_text()
            if code != 0 or f'test {test} ... ok' not in baseline:
                raise RuntimeError(f'{name}: baseline must execute and pass the named test')
            try:
                source.write_text(original.replace(needle, replacement))
                if run(args + ['--no-run'], copy, env, OUT / f'{name}-build.log') != 0:
                    raise RuntimeError(f'{name}: compilation failure is not a killed mutant')
                code = run(args + ['--', '--exact'], copy, env, OUT / f'{name}-mutant.log')
                output = (OUT / f'{name}-mutant.log').read_text()
                if code != 101 or f'test {test} ... FAILED' not in output:
                    raise RuntimeError(f'{name}: expected test did not kill the compiled mutant')
                results.append({'name': name, 'source': relative,
                                'sha256': hashlib.sha256(original.encode()).hexdigest(),
                                'test': test, 'compiled': True, 'killed': True})
            finally:
                source.write_text(original)
    receipt['passed'] = True
    (OUT / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print('Three compiled semantic mutants killed; baselines passed. See', OUT)


if __name__ == '__main__':
    main()
