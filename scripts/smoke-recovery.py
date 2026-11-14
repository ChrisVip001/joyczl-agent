#!/usr/bin/env python3
"""Kill real app-server processes at durable boundaries; all services are local fakes."""
from __future__ import annotations

import argparse
import asyncio
import json
import os
from pathlib import Path
import signal
import sqlite3
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MCP_PROGRAM = r'''
import json,os,sys,time,threading
from pathlib import Path
root=Path(sys.argv[1]); mode=sys.argv[2]; parent=os.getppid()
def watch():
 while os.getppid()==parent: time.sleep(0.05)
 os._exit(0)
threading.Thread(target=watch,daemon=True).start()
def append(name):
 with open(root/name,'a') as f: f.write('1\n'); f.flush(); os.fsync(f.fileno())
for line in sys.stdin:
 m=json.loads(line)
 if 'id' not in m: continue
 method=m.get('method')
 if method=='initialize': result={'protocolVersion':'2025-06-18','capabilities':{'tools':{}},'serverInfo':{'name':'crash-fixture','version':'1'}}
 elif method=='tools/list': result={'tools':[{'name':'commit','description':'Commit one synthetic local record and return its receipt.','inputSchema':{'type':'object','properties':{'value':{'type':'string'}},'required':['value']}}]}
 elif method=='tools/call':
  append('requests'); append('commits')
  if mode=='after_effect':
   (root/'effect-ready').touch()
   while True: time.sleep(0.05)
  result={'content':[{'type':'text','text':'Committed synthetic record; receipt local-1'}]}
 else: result={}
 print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':result}),flush=True)
'''
HOOK_PROGRAM = '''import os,sys,time
from pathlib import Path
sys.stdin.read()
parent=os.getppid()
Path(sys.argv[1]).touch()
while os.getppid()==parent: time.sleep(0.05)
'''


def count(path: Path) -> int:
    return len(path.read_text().splitlines()) if path.exists() else 0


class ProviderFixture:
    def __init__(self, root: Path, mode: str):
        self.root, self.mode = root, mode
        self.release = threading.Event()
        self.calls: list[str] = []
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                results = any(block.get('type') == 'tool_result'
                              for message in body.get('messages', [])
                              for block in message.get('content', []) if isinstance(block, dict))
                if 'You distill a personal assistant' in json.dumps(body.get('system', '')):
                    stage = 'memory'
                    if fixture.mode == 'memory_claim' and not fixture.release.is_set():
                        (fixture.root / 'memory-ready').touch()
                        fixture.release.wait(30)
                    blocks = [{'type': 'text', 'text': json.dumps({'facts': [], 'states': [], 'episode': 'One synthetic task completed.'})}]
                elif not body.get('tools'):
                    stage, text = 'gate', '{"retrieve":false,"query":"","reason":"local fixture"}'
                    blocks = [{'type': 'text', 'text': text}]
                elif results:
                    stage = 'final'
                    blocks = [{'type': 'text', 'text': 'The synthetic result is ready for verification.'}]
                    if fixture.mode == 'after_checkpoint' and not fixture.release.is_set():
                        (fixture.root / 'checkpoint-ready').touch()
                        fixture.release.wait(30)
                else:
                    stage = 'tool'
                    blocks = [{'type': 'tool_use', 'id': 'commit-once', 'name': 'fault_commit',
                               'input': {'value': 'synthetic'}}]
                fixture.calls.append(stage)
                stop = 'tool_use' if stage == 'tool' else 'end_turn'
                if body.get('stream'):
                    events = [('message_start', {'type': 'message_start', 'message': {'usage': {'input_tokens': 10}}})]
                    block = blocks[0]
                    initial = {'type': block['type']}
                    if block['type'] == 'tool_use':
                        initial.update(id=block['id'], name=block['name'], input={})
                        delta = {'type': 'input_json_delta', 'partial_json': json.dumps(block['input'])}
                    else:
                        delta = {'type': 'text_delta', 'text': block['text']}
                    events.extend([
                        ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': initial}),
                        ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': delta}),
                        ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
                        ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': stop}, 'usage': {'output_tokens': 5}}),
                        ('message_stop', {'type': 'message_stop'}),
                    ])
                    data = ''.join(f'event: {name}\ndata: {json.dumps(value)}\n\n' for name, value in events).encode()
                    content_type = 'text/event-stream'
                else:
                    data = json.dumps({'content': blocks, 'stop_reason': stop,
                                       'usage': {'input_tokens': 10, 'output_tokens': 5}}).encode()
                    content_type = 'application/json'
                try:
                    self.send_response(200)
                    self.send_header('content-type', content_type)
                    self.send_header('content-length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


class App:
    def __init__(self, process, log):
        self.process, self.log = process, log
        self.pending = {}
        self.sequence = 0
        self.reader = asyncio.create_task(self.read())

    @classmethod
    async def start(cls, binary: Path, home: Path, workspace: Path):
        env = {name: os.environ[name] for name in ('PATH', 'HOME', 'TMPDIR', 'LANG') if name in os.environ}
        env.update(JOY_HOME=str(home), JOY_PROVIDER='anthropic', JOY_RECOVERY_TEST_KEY='synthetic-only',
                   JOY_LLM_RETRIES='0', JOY_LLM_TIMEOUT='10', JOY_RESUME_MAX_AGE='0',
                   JOY_CONSOLIDATE_EVERY='1' if (home / 'memory-crash').exists() else '1000',
                   JOY_HOOKS='1' if (home / 'hooks.json').exists() else '0', JOY_HOOKS_TIMEOUT='60')
        log = open(home / 'process.log', 'ab')
        process = await asyncio.create_subprocess_exec(str(binary), 'app-server', cwd=workspace,
                    env=env, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                    stderr=log, start_new_session=True)
        app = cls(process, log)
        try:
            await app.request('config/read', {})
        except BaseException:
            await app.kill()
            raise
        return app

    async def read(self):
        async for raw in self.process.stdout:
            message = json.loads(raw)
            if 'id' in message and message['id'] in self.pending:
                future = self.pending.pop(message['id'])
                if future.done():
                    continue
                if 'error' in message:
                    future.set_exception(RuntimeError(message['error']['message']))
                else:
                    future.set_result(message.get('result'))
        for future in self.pending.values():
            future.cancel()
        self.pending.clear()

    async def send(self, method, params):
        self.sequence += 1
        future = asyncio.get_running_loop().create_future()
        self.pending[self.sequence] = future
        message = dict(jsonrpc='2.0', id=self.sequence, method=method, params=params)
        self.process.stdin.write((json.dumps(message) + '\n').encode())
        await self.process.stdin.drain()
        return future

    async def request(self, method, params):
        return await asyncio.wait_for(await self.send(method, params), timeout=15)

    async def kill(self):
        if self.process.returncode is None:
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        await self.process.wait()
        await asyncio.gather(self.reader, return_exceptions=True)
        self.log.close()


async def wait_for(test, description, timeout=20):
    async def poll():
        while not test():
            await asyncio.sleep(0.02)
    try:
        await asyncio.wait_for(poll(), timeout)
    except asyncio.TimeoutError as error:
        raise AssertionError(f'Timed out waiting for {description}') from error


async def status(app, task, expected):
    async def poll():
        while True:
            view = await app.request('task/get', {'taskId': task})
            current = view['task']['status']
            if current == expected:
                return view['task']
            if current in ('done', 'needs_attention', 'cancelled'):
                raise AssertionError(f'Expected {expected}, got {current}: {view["task"].get("lastStopReason")}')
            await asyncio.sleep(0.05)
    return await asyncio.wait_for(poll(), timeout=20)


async def run_case(binary: Path, mode: str, baseline: dict | None):
    with tempfile.TemporaryDirectory(prefix='joy-crash-') as temp:
        root = Path(temp)
        home, workspace = root / 'home', root / 'workspace'
        home.mkdir(); workspace.mkdir()
        provider = ProviderFixture(root, mode)
        (root / 'mcp.py').write_text(MCP_PROGRAM)
        (root / 'hook.py').write_text(HOOK_PROGRAM)
        (home / 'models.json').write_text(json.dumps({'active': 'fixture', 'profiles': [
            {'id': 'fixture', 'provider': 'anthropic', 'model': 'fixture', 'small_model': 'fixture',
             'base_url': f'http://127.0.0.1:{provider.server.server_port}', 'key_source': 'env:JOY_RECOVERY_TEST_KEY'}]}))
        (home / 'mcp.json').write_text(json.dumps({'servers': [
            {'name': 'fault', 'command': sys.executable, 'args': [str(root / 'mcp.py'), str(root), mode]}]}))
        if mode == 'before_request':
            (home / 'hooks.json').write_text(json.dumps({'hooks': {'PreToolUse': [
                {'matcher': 'fault_commit', 'command': sys.executable,
                 'args': [str(root / 'hook.py'), str(root / 'hook-ready')], 'timeout': 60}]}}))
        if mode == 'memory_claim':
            (home / 'memory-crash').touch()
        app = None
        memory_recovery = None
        try:
            app = await App.start(binary, home, workspace)
            created = await app.request('task/create', {'sessionId': 'fault', 'title': 'Synthetic crash recovery',
                'cwd': str(workspace), 'maxRounds': 2, 'acceptance': [{'kind': 'toolsOk', 'tool': 'fault_commit', 'min': 1}]})
            task = created['task']['id']
            turn = await app.send('turn/start', {'sessionId': 'fault', 'message': 'Perform one synthetic commit and verify it.',
                                               'cwd': str(workspace), 'stream': False})
            original_run = None
            if mode in ('normal', 'memory_claim'):
                await asyncio.wait_for(turn, timeout=20)
                result = await status(app, task, 'done')
                if mode == 'memory_claim':
                    await wait_for(lambda: (root / 'memory-ready').exists(), 'memory extraction admission')
                    with sqlite3.connect(home / 'state.db') as database:
                        claimed = database.execute('SELECT COUNT(*) FROM chat_log WHERE consolidation_claimed_at IS NOT NULL').fetchone()[0]
                        memory_batch = database.execute('SELECT consolidation_batch FROM chat_log LIMIT 1').fetchone()[0]
                    assert claimed == 2, 'Memory crash is not inside an admitted source batch'
                    await app.kill(); app = None
                    provider.release.set()
                    app = await App.start(binary, home, workspace)
                    def memory_is_recovered():
                        nonlocal memory_recovery
                        with sqlite3.connect(home / 'state.db') as database:
                            claimed = database.execute('SELECT COUNT(*) FROM chat_log WHERE consolidation_claimed_at IS NOT NULL').fetchone()[0]
                            pending = database.execute('SELECT COUNT(*) FROM chat_log WHERE consolidated=0').fetchone()[0]
                            completed = database.execute('SELECT COUNT(*) FROM chat_log WHERE consolidated=1 AND consolidation_claimed_at IS NULL').fetchone()[0]
                            summaries = database.execute('SELECT COUNT(*) FROM memory_episode_contexts').fetchone()[0]
                            same_batch = database.execute('SELECT COUNT(*) FROM chat_log WHERE consolidation_batch=?', (memory_batch,)).fetchone()[0]
                        memory_recovery = dict(pendingRows=pending, completedRows=completed, summaries=summaries, sourceBatchPreserved=same_batch == 2)
                        return claimed == 0 and pending + completed == 2 and summaries <= 1 and same_batch == 2
                    await wait_for(memory_is_recovered, 'memory source claim recovery')
                    result = await status(app, task, 'done')
            else:
                marker = {'before_request': 'hook-ready', 'after_effect': 'effect-ready',
                          'after_checkpoint': 'checkpoint-ready'}[mode]
                await wait_for(lambda: (root / marker).exists(), marker)
                with sqlite3.connect(home / 'state.db') as database:
                    row = database.execute('SELECT messages_json,usage_json,run_id FROM turn_checkpoints WHERE session_id=?', ('fault',)).fetchone()
                    original_run = row[2]
                    assert original_run, 'Crash checkpoint has no execution identity'
                    checkpoint_usage = json.loads(row[1])
                    known_tokens = checkpoint_usage.get('input_tokens', 0) + checkpoint_usage.get('output_tokens', 0)
                    saved_result = any(block.get('type') == 'tool_result'
                                       for message in json.loads(row[0]) for block in message['content'])
                assert saved_result == (mode == 'after_checkpoint'), 'Crash marker is not at the intended durable boundary'
                assert count(root / 'requests') == (0 if mode == 'before_request' else 1), 'Unexpected pre-crash tool dispatch count'
                await app.kill(); app = None
                provider.release.set()
                app = await App.start(binary, home, workspace)
                result = await status(app, task, 'done' if mode == 'after_checkpoint' else 'needs_attention')
                if mode != 'after_checkpoint':
                    assert result['spent']['tokens'] >= known_tokens, f"Known checkpoint tokens were lost: {known_tokens} known, {result['spent']['tokens']} charged"
                    charged = result['spent']['tokens']
                    await app.kill(); app = None
                    app = await App.start(binary, home, workspace)
                    result = await status(app, task, 'needs_attention')
                    assert result['spent']['tokens'] == charged, 'Repeated recovery charged known usage twice'

            effects = count(root / 'commits')
            requests = count(root / 'requests')
            assert effects == requests == (0 if mode == 'before_request' else 1), 'A tool was replayed after process recovery'
            runs = await app.request('task/runs', {'taskId': task})
            assert len(runs['runs']) == 1, 'Recovery created another task run'
            if original_run is not None:
                assert runs['runs'][0]['id'] == original_run, 'Recovery replaced the original execution'
            if mode == 'after_checkpoint':
                assert result['roundsUsed'] == baseline['roundsUsed'], 'Recovery lost or duplicated the round counter'
                assert result['spent']['tokens'] == baseline['spentTokens'], 'Recovery lost or duplicated known token spend'
            return {'case': mode, 'status': result['status'], 'toolRequests': requests, 'effects': effects,
                    'rounds': result['roundsUsed'], 'tokens': result['spent']['tokens'], 'checks': result['checks'],
                    'memoryRecovery': memory_recovery}
        except Exception:
            log = home / 'process.log'
            if log.exists():
                print(log.read_text()[-12000:], file=sys.stderr)
            raise
        finally:
            if app:
                await app.kill()
            provider.close()


async def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', type=Path, default=Path(__file__).resolve().parents[1] / 'joy-rs/target/debug/joy')
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    if os.name != 'posix':
        print('SKIP: process-group crash smoke requires POSIX')
        return
    binary = args.bin.resolve()
    assert binary.is_file(), 'Build the joy binary before running recovery smoke'
    results = []
    baseline = None
    for mode in ('normal', 'before_request', 'after_effect', 'after_checkpoint', 'memory_claim'):
        result = await run_case(binary, mode, baseline)
        if mode == 'normal':
            baseline = {'roundsUsed': result['rounds'], 'spentTokens': result['tokens']}
        results.append(result)
        print(f'PASS {mode}: {result["effects"]} effects, {result["status"]}', flush=True)
    if args.report:
        args.report.write_text(json.dumps({'provider': 'local-scripted-fixture', 'results': results}, indent=2) + '\n')


if __name__ == '__main__':
    asyncio.run(main())
