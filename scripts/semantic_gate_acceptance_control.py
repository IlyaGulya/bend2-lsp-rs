#!/usr/bin/env python3
"""Diagnostic-only fault controls; execute the original gate and keep its failure."""
import argparse
import base64
import gzip
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path.cwd()
KINDS = ('semantic-mismatch', 'missing-oracle', 'omitted-case', 'hard-budget')


def checksum(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def store(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def run(kind, output):
    output.mkdir(parents=True, exist_ok=False)
    sys.path.insert(0, str(ROOT / 'scripts'))
    import semantic_gate as gate
    import semantic_install as installer
    candidate = gate.default_candidate().resolve()
    original_script = ROOT / 'scripts/semantic_conformance.py'
    receipt = {
        'schema': 1, 'kind': kind, 'injected_control_not_a_production_bug': True,
        'source_sha': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'workflow_sha': os.environ['GITHUB_SHA'],
        'profile_sha256': checksum(gate.PROFILE_PATH),
        'original_candidate_sha256': checksum(candidate),
        'original_conformance_sha256': checksum(original_script),
        'command': [sys.executable, 'scripts/semantic_gate.py'],
    }
    runtime = None
    unavailable = None
    if kind == 'semantic-mismatch':
        directory = output / 'self-contained-candidate'
        directory.mkdir()
        encoded = base64.b64encode(gzip.compress(candidate.read_bytes(), mtime=0)).decode('ascii')
        wrapper = '''#!/usr/bin/env python3
import base64,gzip,json,os,subprocess,sys,tempfile
with tempfile.TemporaryDirectory(prefix='semantic-negative-candidate-') as directory:
    binary=os.path.join(directory,'probe')
    with open(binary,'wb') as stream:stream.write(gzip.decompress(base64.b64decode(EMBEDDED)))
    os.chmod(binary,0o700)
    child=subprocess.Popen([binary],stdin=subprocess.PIPE,stdout=subprocess.PIPE)
    try:
        for line in sys.stdin.buffer:
            child.stdin.write(line);child.stdin.flush()
            raw=child.stdout.readline()
            if not raw:raise RuntimeError('actual native probe ended without a reply')
            reply=json.loads(raw)
            if reply.get('outcome')=='valid':
                for occurrence in reply.get('occurrences',[]):
                    if occurrence.get('role')=='call' and occurrence.get('target') is not None:
                        occurrence['target']=None;occurrence['status']='unresolved';break
            print(json.dumps(reply,separators=(',',':')),flush=True)
    finally:
        child.stdin.close()
        try:child.wait(timeout=1)
        except subprocess.TimeoutExpired:child.kill();child.wait(timeout=1)
'''.replace('EMBEDDED', repr(encoded))
        candidate = directory / 'semantic_probe'
        candidate.write_text(wrapper)
        candidate.chmod(0o700)
        (directory / 'bend2-lsp').symlink_to(gate.default_candidate().resolve().parent / 'bend2-lsp')
        receipt['controlled_candidate_sha256'] = checksum(candidate)
        receipt['command'] += ['scheduled', '--candidate', str(candidate), '--output', str(output / 'gate')]
    else:
        receipt['command'] += ['run', '--output', str(output / 'gate')]
        if kind == 'missing-oracle':
            runtime = Path(installer.runtime_path())
            unavailable = runtime.with_name('bun-intentionally-unavailable')
            receipt['authenticated_runtime_sha256'] = checksum(runtime)
            runtime.rename(unavailable)
            if runtime.exists() or checksum(unavailable) != receipt['authenticated_runtime_sha256']:
                raise ValueError('missing-oracle control did not hide the exact authenticated runtime')
            receipt['runtime_missing_during_cli'] = True
        else:
            retained = original_script.with_name('semantic_conformance_control_original.py')
            retained.write_bytes(original_script.read_bytes())
            receipt['retained_conformance_sha256'] = checksum(retained)
            if kind == 'omitted-case':
                injection = '''
if len(sys.argv)>1 and sys.argv[1]=='plan':
    original_plan=namespace['make_plan']
    def omitted(*args,**kwargs):
        plan=original_plan(*args,**kwargs)
        removed=plan['planned'].pop()
        plan.pop('manifest_sha256')
        plan['manifest_sha256']=namespace['digest'](namespace['canonical'](plan))
        return plan
    namespace['main'].__globals__['make_plan']=omitted
'''
            else:
                injection = '''
if len(sys.argv)>1 and sys.argv[1]=='plan':
    import signal,subprocess,time
    signal.signal(signal.SIGTERM,signal.SIG_IGN)
    child=subprocess.Popen([sys.executable,'-c','import signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(600)'])
    Path(OWNED_PID_RECEIPT).write_text(json.dumps({'planner':os.getpid(),'child':child.pid}))
    time.sleep(600)
'''.replace('OWNED_PID_RECEIPT', repr(str(output / 'owned-pids.json')))
            replacement = (
                'import json,os,runpy,sys\nfrom pathlib import Path\n'
                + 'namespace=runpy.run_path(' + repr(str(retained)) + ')\n'
                + injection + '\nsys.exit(namespace["main"]())\n'
            )
            original_script.write_text(replacement)
            receipt['injected_conformance_sha256'] = checksum(original_script)
    store(output / 'control-receipt.json', receipt)
    try:
        with (output / 'original-cli.log').open('wb') as log:
            process = subprocess.run(receipt['command'], stdout=log, stderr=subprocess.STDOUT, timeout=3660)
        receipt['actual_cli_exit'] = process.returncode
        store(output / 'control-receipt.json', receipt)
        return process.returncode
    finally:
        if unavailable is not None:
            unavailable.rename(runtime)


def verify(kind, output):
    receipt = json.loads((output / 'control-receipt.json').read_text())
    if receipt.get('actual_cli_exit') not in (1, 2):
        raise ValueError('original gate did not terminate with the required nonzero failure')
    reports = list((output / 'gate').rglob('gate-report.json'))
    if len(reports) != 1:
        raise ValueError('expected exactly one authentic failed gate report')
    report = json.loads(reports[0].read_text())
    if report['status'] != 'error':
        raise ValueError('injected failure became semantic pass')
    for stage in report['stages'].values():
        if stage.get('cleanup_errors'):
            raise ValueError('owned stage children were not cleanly reaped')
        if stage.get('returncode') is None:
            raise ValueError('owned stage process has no terminal returncode')
    original_bundles = list((output / 'gate').rglob('original.json'))
    details = {'schema': 1, 'kind': kind, 'verified': True, 'actual_cli_exit': receipt['actual_cli_exit'],
               'gate_report_sha256': checksum(reports[0]), 'gate_status': report['status'],
               'stage_statuses': {name: stage['status'] for name, stage in report['stages'].items()}}
    if kind == 'semantic-mismatch':
        scheduled = json.loads((output / 'gate/scheduled-report.json').read_text())
        workers = list((output / 'gate').rglob('worker-report.json'))
        worker = json.loads(workers[0].read_text()) if len(workers) == 1 else {}
        if scheduled['status'] != 'error' or not original_bundles:
            raise ValueError('scheduled failure lost its original counterexample')
        if not any(failure['class'] == 'semantic-mismatch' for failure in worker.get('failures', [])):
            raise ValueError('actual wrong edge did not reach the semantic comparator')
        shrink = scheduled.get('extended_shrink')
        if not shrink or shrink['status'] != 'pass' or shrink.get('cleanup_errors'):
            raise ValueError('real scheduled extended replay/shrink did not complete cleanly')
        sys.path.insert(0, str(ROOT / 'scripts'))
        import semantic_replay
        bundles = []
        for original in original_bundles:
            semantic_replay.load_bundle(original.parent)
            minimized = original.parent / 'minimized.json'
            if not minimized.is_file():
                raise ValueError('scheduled extended reducer did not retain its result')
            reduced = json.loads(minimized.read_text())
            if reduced['status'] not in ('local-minimum', 'time-budget', 'attempt-budget'):
                raise ValueError('extended reducer has no valid explicit completion status')
            if checksum(minimized) != (original.parent / 'minimized.sha256').read_text().strip():
                raise ValueError('extended minimized artifact checksum mismatch')
            if not 0 <= reduced['attempts'] <= 2000:
                raise ValueError('extended reducer exceeded its unchanged attempt cap')
            bundles.append({'path': str(original.parent), 'original_sha256': checksum(original),
                            'minimized_sha256': checksum(minimized), 'shrink_status': reduced['status'],
                            'attempts': reduced['attempts']})
        details['bundles'] = bundles
        details['scheduled_failure_preserved_after_successful_shrink'] = True
    elif kind == 'omitted-case':
        log = '\n'.join(path.read_text(errors='replace') for path in (output / 'gate').rglob('*.log'))
        if 'stale-plan-or-pin' not in log:
            raise ValueError('omitted planned case was not rejected by exact regenerated plan')
        plan = json.loads(next((output / 'gate').rglob('plan.json')).read_text())
        if len(plan['planned']) + 1 != plan['counts']['planned']:
            raise ValueError('control did not actually omit exactly one planned view')
        details['omitted_views'] = 1
    elif kind == 'missing-oracle':
        log = '\n'.join(path.read_text(errors='replace') for path in (output / 'gate').rglob('*.log'))
        if 'runtime binary does not match authenticated release archive' not in log:
            raise ValueError('missing authenticated oracle was not the observed cause')
        if not receipt.get('runtime_missing_during_cli'):
            raise ValueError('control did not establish that the authenticated runtime was missing')
        if report['stages']['plan']['status'] != 'pass' or report['stages']['semantic']['status'] != 'error':
            raise ValueError('missing oracle did not fail the actual semantic stage')
    else:
        stage = report['stages']['plan']
        if 'deadline' not in (stage.get('error') or ''):
            raise ValueError('full unchanged hard deadline did not reject the unresponsive planner')
        owned = json.loads((output / 'owned-pids.json').read_text())
        for pid in owned.values():
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                pass
            else:
                raise ValueError('owned deadline-control process remains alive or unreaped: ' + str(pid))
        details['owned_processes_dead'] = owned
    store(output / 'verification.json', details)
    print(json.dumps(details, sort_keys=True))
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=('run', 'verify'))
    parser.add_argument('kind', choices=KINDS)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    return (run if args.action == 'run' else verify)(args.kind, args.output.resolve())


if __name__ == '__main__':
    sys.exit(main())
