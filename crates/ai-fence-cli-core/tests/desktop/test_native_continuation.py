"""Bounded native-client protocol fixture: isolated state, fake auth, loopback only."""
import base64
import hashlib
import json
import os
from pathlib import Path
import queue
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

def run_native_continuation(native, error_code):
    calls, problems = [], []
    failure_index = [None]
    class Handler(BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'
        def log_message(self, *args):
            pass
        def do_GET(self):
            if self.headers.get('Upgrade', '').lower() != 'websocket':
                data = b'{"models":[],"data":[]}'
                self.send_response(200)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            key = self.headers['Sec-WebSocket-Key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'
            self.send_response(101)
            self.send_header('Upgrade', 'websocket')
            self.send_header('Connection', 'Upgrade')
            self.send_header('Sec-WebSocket-Accept', base64.b64encode(hashlib.sha1(key.encode()).digest()).decode())
            self.end_headers()
            self.connection.settimeout(25)
            def exact(n):
                data = self.rfile.read(n)
                if len(data) != n:
                    raise EOFError()
                return data
            def send(value, opcode=1):
                data = value if isinstance(value, bytes) else json.dumps(value).encode()
                head = bytes([0x80 | opcode])
                head += bytes([len(data)]) if len(data) < 126 else bytes([126]) + struct.pack('!H',len(data))
                self.wfile.write(head + data)
                self.wfile.flush()
            try:
                while True:
                    first, second = exact(2)
                    size = second & 127
                    if size == 126:
                        size = struct.unpack('!H', exact(2))[0]
                    elif size == 127:
                        size = struct.unpack('!Q', exact(8))[0]
                    assert size < 4*1024*1024
                    mask = exact(4) if second & 128 else b''
                    data = exact(size)
                    if mask:
                        data = bytes(b ^ mask[i % 4] for i,b in enumerate(data))
                    opcode = first & 15
                    if opcode == 8:
                        return
                    if opcode == 9:
                        send(data,10)
                        continue
                    if opcode != 1:
                        continue
                    request = json.loads(data)
                    calls.append(request)
                    if failure_index[0] is None and request.get('previous_response_id') and 'Continue fixture task' in json.dumps(request.get('input')):
                        failure_index[0] = len(calls)-1
                        send({'type':'error','status':400,'error':{'type':'invalid_request_error','code':error_code,'param':'previous_response_id','message':'Fixture previous response is unavailable'}})
                        continue
                    rid = 'resp_fixture_' + str(len(calls))
                    message = {'type':'message','id':'msg_'+rid,'status':'completed','role':'assistant','content':[{'type':'output_text','text':'Fixture completed.','annotations':[]}]}
                    response = {'id':rid,'object':'response','created_at':0,'status':'in_progress','model':'gpt-5.5','output':[]}
                    send({'type':'response.created','response':response})
                    send({'type':'response.output_item.added','output_index':0,'item':dict(message,status='in_progress',content=[])})
                    send({'type':'response.output_text.delta','item_id':message['id'],'output_index':0,'content_index':0,'delta':'Fixture completed.'})
                    send({'type':'response.output_item.done','output_index':0,'item':message})
                    response.update(status='completed',output=[message],usage={'input_tokens':1,'output_tokens':1,'total_tokens':2})
                    send({'type':'response.completed','response':response})
            except (EOFError,ConnectionError,socket.timeout):
                return
            except Exception as error:
                problems.append(type(error).__name__)
        def do_POST(self):
            # Unexpected transport is bounded and never forwarded elsewhere.
            size = int(self.headers.get('Content-Length','0'))
            self.rfile.read(min(size,4*1024*1024))
            self.send_response(400)
            self.send_header('Content-Length','0')
            self.end_headers()
            problems.append('unexpected_http_post')
    server = ThreadingHTTPServer(('127.0.0.1',0),Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever,daemon=True).start()
    result = {}
    with tempfile.TemporaryDirectory(prefix='af-native-continuation-') as temporary:
        root = Path(temporary)
        state, home, project = (root/n for n in ('state','home','project'))
        for p in (state,home,project):
            p.mkdir()
        (state/'config.toml').write_text('model="gpt-5.5"\nmodel_provider="fixture"\napproval_policy="never"\nsandbox_mode="read-only"\n[features]\nresponses_websockets=true\n[model_providers.fixture]\nname="Isolated native recovery fixture"\nwire_api="responses"\nsupports_websockets=true\nrequires_openai_auth=false\nenv_key="AF_FIXTURE_KEY"\nbase_url="http://127.0.0.1:'+str(server.server_port)+'/v1"\n')
        env={k:v for k,v in os.environ.items() if not k.startswith(('AI_FENCE_','OPENAI_','ANTHROPIC_','CODEX_'))}
        env.update(HOME=str(home),CODEX_HOME=str(state),AF_FIXTURE_KEY='fixture-invalid-no-provider-access',OTEL_SDK_DISABLED='true',HTTP_PROXY='http://127.0.0.1:1',HTTPS_PROXY='http://127.0.0.1:1',ALL_PROXY='http://127.0.0.1:1',NO_PROXY='127.0.0.1,localhost')
        process = subprocess.Popen([str(native),'app-server'],cwd=project,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        messages, errors = queue.Queue(), []
        def read():
            for line in process.stdout:
                try:
                    messages.put(json.loads(line))
                except ValueError:
                    problems.append('invalid_native_json')
            messages.put(None)
        def stderr():
            # Synthetic fixture only, retained privately without printing bodies.
            for line in process.stderr:
                errors.append(line)
        reader = threading.Thread(target=read, daemon=True)
        error_reader = threading.Thread(target=stderr, daemon=True)
        reader.start()
        error_reader.start()
        def send(value):
            process.stdin.write(json.dumps(value).encode()+b'\n')
            process.stdin.flush()
        def wait(predicate):
            deadline=time.monotonic()+30
            while time.monotonic()<deadline:
                value=messages.get(timeout=max(.01,deadline-time.monotonic()))
                if value is None:
                    raise RuntimeError('native_closed')
                if predicate(value):
                    return value
            raise RuntimeError('native_timeout')
        def rpc(i,method,params):
            send(dict(id=i,method=method,params=params))
            v=wait(lambda v:v.get('id')==i)
            if 'error' in v:
                raise RuntimeError('rpc_rejected_'+method+'_'+str(v['error'].get('code')))
            return v['result']
        try:
            rpc(1,'initialize',{'clientInfo':{'name':'af_native_recovery_fixture','version':'1'}})
            send({'method':'initialized','params':{}})
            thread=rpc(2,'thread/start',{'cwd':str(project)})['thread']['id']
            for i in (3,4):
                rpc(i,'turn/start',{'threadId':thread,'input':[{'type':'text','text':'Original fixture task' if i==3 else 'Continue fixture task'}]})
                completed=wait(lambda v:v.get('method')=='turn/completed')
                if completed.get('params',{}).get('turn',{}).get('status') != 'completed':
                    raise RuntimeError('native_turn_not_completed')
            index=failure_index[0]
            replay=calls[index+1] if index is not None and index+1<len(calls) else {}
            result=dict(synthetic=True,websocket_calls=len(calls),
                failure_injected=index is not None,
                recovery_omits_previous_id=bool(replay and not replay.get('previous_response_id')),
                recovery_retains_original_task=bool(replay and 'Original fixture task' in json.dumps(replay.get('input'))),
                exactly_one_recovery_request=bool(index is not None and len(calls)==index+2),
                problems=problems)
            assert result['failure_injected'] and result['exactly_one_recovery_request'] and result['recovery_omits_previous_id'] and result['recovery_retains_original_task'] and not problems
        except Exception as error:
            result=dict(synthetic=True,failure=type(error).__name__,reason=str(error)[:160],websocket_calls=len(calls),problems=problems)
        finally:
            process.stdin.close()
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)
            reader.join(timeout=2)
            error_reader.join(timeout=2)
            process.stdout.close()
            process.stderr.close()
            server.shutdown()
            server.server_close()
    return result

@unittest.skipUnless(os.environ.get("AF_TEST_NATIVE"), "set AF_TEST_NATIVE")
class NativeContinuationRecovery(unittest.TestCase):
    def test_native_full_history_recovery_requires_the_standard_error_code(self):
        native = Path(os.environ["AF_TEST_NATIVE"]).resolve(strict=True)
        rejected = run_native_continuation(native, "invalid_request_error")
        self.assertEqual(rejected.get("reason"), "native_turn_not_completed", rejected)
        self.assertEqual(rejected["problems"], [])
        recovered = run_native_continuation(native, "previous_response_not_found")
        self.assertNotIn("failure", recovered, recovered)
        self.assertTrue(recovered["failure_injected"])
        self.assertTrue(recovered["exactly_one_recovery_request"])
        self.assertTrue(recovered["recovery_omits_previous_id"])
        self.assertTrue(recovered["recovery_retains_original_task"])
        self.assertEqual(recovered["problems"], [])
        self.assertEqual(recovered["websocket_calls"], rejected["websocket_calls"] + 1)


if __name__ == "__main__":
    unittest.main()
