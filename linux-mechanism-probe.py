import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import sys
import tempfile
import time
sys.path.insert(0, 'scripts')
from semantic_lsp import LspProcess

class TraceProcess(LspProcess):
    def __init__(self, binary, workspace):
        self.notifications = []
        super().__init__(binary, workspace)
    def _dispatch(self, body, received_ns):
        message = json.loads(body)
        if message.get('method') in ('window/logMessage', 'window/showMessage'):
            self.notifications.append(message)
        return super()._dispatch(body, received_ns)

binary, fixture, wrapper = map(Path, sys.argv[1:])
source = 'def main(+x: U32) -> U32:\n  x\n'
compiler_error = 'Error:\nselected compiler rejection\nLocation: main\n1>| def main(+x: U32) -> U32:\n  |     ^^^^\n2 |   x\n'
results = []
for delay_ms in (0, 750):
    with tempfile.TemporaryDirectory(prefix='c7-lint-startup-mechanism-') as directory:
        root = Path(directory)
        project = root / 'project with spaces'
        control = root / 'control'
        project.mkdir()
        control.mkdir()
        document = project / 'main.bend'
        document.write_text(source)
        (project / 'bend-lint.json').write_text('{"rules":{}}')
        for name, content in {'response.json':'{"ok":true,"findings":[],"suppressed":[]}', 'exit-code':'0', 'compiler.stderr':compiler_error, 'spawn-descendant':'', 'block':''}.items():
            (control / name).write_text(content)
        os.environ.update(BEND_LINT_FIXTURE_CONTROL=str(control), BEND_LINT_FIXTURE_ORIGINAL=str(fixture), BEND_LINT_FIXTURE_DELAY_MS=str(delay_ms))
        client = TraceProcess(binary, project)
        client.settings = {'bend2-lsp':{'compilerPath':str(fixture),'compilerArguments':[]},'bend':{'lint':{'enabled':True,'path':str(wrapper),'timeoutMs':500}}}
        def until(predicate):
            deadline = time.monotonic() + 10
            while not predicate():
                client._receive(deadline)
        def compiler_visible(version):
            packet = client.diagnostics.get(document.as_uri(), {})
            return packet.get('version') == version and any('selected compiler rejection' in item.get('message','') for item in packet.get('diagnostics',[]))
        try:
            client.initialize()
            client.notify('textDocument/didOpen', {'textDocument':{'uri':document.as_uri(),'languageId':'bend','version':1,'text':source}})
            until(lambda: compiler_visible(1))
            client.notify('textDocument/didSave', {'textDocument':{'uri':document.as_uri()}})
            until(lambda: any('bend-lint' in message.get('params',{}).get('message','').lower() and 'timed out' in message.get('params',{}).get('message','').lower() for message in client.notifications))
            failure = next(message for message in client.notifications if 'timed out' in message.get('params',{}).get('message','').lower())
            pid = int((control / 'wrapper-started').read_text())
            deadline = time.monotonic() + 10
            while True:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                assert time.monotonic() < deadline, 'timed-out analyzer not reaped'
                time.sleep(0.01)
            addresses = {name:(control / name).read_text() for name in ('address','descendant-address') if (control / name).exists()}
            if delay_ms == 0:
                assert set(addresses) == {'address','descendant-address'}, addresses
                for address in addresses.values():
                    host, port = address.strip().split(':')
                    try:
                        connection = socket.create_connection((host,int(port)), timeout=0.1)
                    except OSError:
                        pass
                    else:
                        connection.close()
                        raise AssertionError('expired analyzer/descendant still owns listener')
            else:
                assert addresses == {} and not (control / 'calls').exists(), addresses
            client.notify('textDocument/didChange', {'textDocument':{'uri':document.as_uri(),'version':2},'contentChanges':[{'text':source}]})
            until(lambda: compiler_visible(2))
            assert all(item.get('source') != 'bend-lint' for item in client.diagnostics[document.as_uri()]['diagnostics'])
            stderr = client.stderr_tail()
            client.finish()
            results.append({'injected_fixture_prepublication_delay_ms':delay_ms,'original_lint_timeout_ms':500,'actual_timeout_notification':failure,'analyzer_pid_reaped':True,'published_addresses':addresses,'compiler_rejection_still_visible_at_version2':True,'no_fabricated_lint_diagnostic':True,'clean_shutdown':True,'server_stderr':stderr})
        finally:
            (control / 'release').write_text('')
            client.close()
print(json.dumps({'production_binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'original_fixture_source_sha256':hashlib.sha256(Path('tests/fixtures/bend_lint_process.rs').read_bytes()).hexdigest(),'cases':results,'scope':'Controlled Linux functional mechanism only; not a performance experiment or attribution of the original Windows CI failure','original_failed_run_unchanged':40863668428},indent=2))
