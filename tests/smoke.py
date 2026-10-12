#!/usr/bin/env python3
"""Exercise the actual Rust service and CLI without contacting any live sources."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/workboard').resolve())
with tempfile.TemporaryDirectory(prefix='workboard-smoke-') as folder:
    root = Path(folder) / 'data'
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    base = f'http://127.0.0.1:{port}'
    command = [binary, '--data-dir', str(root), '--port', str(port)]
    process = subprocess.Popen(command + ['serve', '--demo'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    def call(path, method='GET', body=None, token=None, cookie=None, origin=None, host=None):
        headers = {}
        if token is not None: headers['Authorization'] = 'Bearer ' + token
        if cookie is not None: headers['Cookie'] = cookie
        if origin is not None: headers['Origin'] = origin
        if host is not None: headers['Host'] = host
        data = None
        if body is not None:
            headers['Content-Type'] = 'application/json'
            data = json.dumps(body).encode()
        request = Request(base + path, data=data, headers=headers, method=method)
        try:
            response = urlopen(request, timeout=5)
            return response.status, response.headers, response.read()
        except HTTPError as error:
            return error.code, error.headers, error.read()
    try:
        for _ in range(100):
            if process.poll() is not None:
                raise RuntimeError('Service exited: ' + process.stderr.read().decode())
            if (root / 'service.token').exists():
                try:
                    if call('/')[0] == 200: break
                except OSError: pass
            time.sleep(.05)
        else: raise RuntimeError('Service did not start')
        token = (root / 'service.token').read_text().strip()
        assert call('/api/snapshot')[0] == 401
        assert call('/api/snapshot', token='invalid')[0] == 401
        assert call('/api/snapshot', token=token, host='attacker.example')[0] == 403
        status, headers, body = call('/api/session', 'POST', token=token)
        assert status == 200
        assert 'HttpOnly' in headers['Set-Cookie'] and 'SameSite=Strict' in headers['Set-Cookie']
        cookie = headers['Set-Cookie'].split(';')[0]
        assert call('/api/snapshot', cookie=cookie)[0] == 200
        assert call('/api/efforts', 'POST', {'title':'CSRF'}, cookie=cookie, origin='https://attacker.example')[0] == 403
        assert call('/api/efforts', 'POST', {'title':'Missing origin'}, cookie=cookie)[0] == 403
        status, _, body = call('/api/snapshot', token=token)
        snapshot = json.loads(body)
        assert len(snapshot['items']) == 6 and len(snapshot['efforts']) == 1
        assert len({i['provider'] for i in snapshot['items']}) == 6
        assert len(json.loads(subprocess.check_output(command + ['next','--json']))) == 4
        without_home = {k:v for k,v in os.environ.items() if k != 'HOME'}
        assert len(json.loads(subprocess.check_output(command + ['next','--json'], env=without_home))) == 4
        status, _, body = call('/api/efforts', 'POST', {'title':'Cross-source follow-up','item_ids':['demo-4','demo-5']}, cookie=cookie, origin=base)
        assert status == 200
        effort = json.loads(body)
        snapshot = json.loads(call('/api/snapshot', token=token)[2])
        assert all(i['effort_id'] == effort['id'] for i in snapshot['items'] if i['id'] in ['demo-4','demo-5'])
        before = len(snapshot['efforts'])
        assert call('/api/efforts', 'POST', {'title':'Rollback','item_ids':['demo-4','missing']}, token=token)[0] == 400
        assert len(json.loads(call('/api/snapshot', token=token)[2])['efforts']) == before
        invalid = {'provider':'errata','key':'1','title':'Bad URL','url':'javascript:alert(1)','status':'Open','action':'next'}
        assert call('/api/items', 'POST', invalid, token=token)[0] == 400
        invalid['url'] = 'https://errata.example.invalid/advisory/1'
        status, _, body = call('/api/items', 'POST', invalid, token=token)
        assert status == 200
        manual = json.loads(body)
        subprocess.check_call(command + ['update',manual['id'],'--status','Released'], stdout=subprocess.DEVNULL)
        snapshot = json.loads(call('/api/snapshot', token=token)[2])
        assert next(i for i in snapshot['items'] if i['id']==manual['id'])['terminal']
        assert token.encode() not in json.dumps(snapshot).encode()
        assert call('/api/connections','POST',{'provider':'jira','base_url':'https://jira.example.invalid','label':'Jira','token':''},token=token)[0] == 400
        if os.name == 'posix':
            assert (root.stat().st_mode & 0o777) == 0o700
            assert ((root / 'service.token').stat().st_mode & 0o777) == 0o600
            assert ((root / 'work.sqlite').stat().st_mode & 0o777) == 0o600
        process.terminate(); process.wait(timeout=5)
        # Restart preserves grouping and rotates the application credential.
        process = subprocess.Popen(command + ['serve'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        for _ in range(100):
            try:
                fresh = (root / 'service.token').read_text().strip()
                if fresh != token and call('/api/snapshot',token=fresh)[0] == 200: break
            except OSError: pass
            time.sleep(.05)
        else: raise RuntimeError('Restart did not complete')
        assert call('/api/snapshot',token=token)[0] == 401
        snapshot = json.loads(call('/api/snapshot',token=fresh)[2])
        assert all(i['effort_id']==effort['id'] for i in snapshot['items'] if i['id'] in ['demo-4','demo-5'])
        print('PASS: authentication, CSRF, host checks, CLI, grouping, rollback, source links, manual completion, permissions and restart persistence')
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
