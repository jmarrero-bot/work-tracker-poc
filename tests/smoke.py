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
        assert len(snapshot['items']) == 7 and len(snapshot['efforts']) == 1
        assert len({i['provider'] for i in snapshot['items']}) == 6
        # Manual items have no connection to make them external.
        assert all(i['zone'] == 'internal' for i in snapshot['items'])
        # So none of them leaves: the export for the hub is empty.
        assert call('/api/export/external')[0] == 401
        exported = subprocess.check_output(command + ['export','--external'], text=True)
        assert json.loads(exported) == {'zone':'external','connections':[],'items':[],'efforts':[]}, exported
        assert subprocess.run(command + ['export'], capture_output=True).returncode != 0
        # Effort titles stay unless shared.
        def demo_sharing():
            [demo] = json.loads(subprocess.check_output(command + ['status','bootc','--json']))
            return demo['effort']['id'], demo['effort']['share_title']
        demo_id, shared = demo_sharing()
        assert shared is False
        subprocess.check_call(command + ['share-title', demo_id], stdout=subprocess.DEVNULL)
        assert demo_sharing() == (demo_id, True)
        subprocess.check_call(command + ['share-title', demo_id, '--undo'], stdout=subprocess.DEVNULL)
        assert demo_sharing() == (demo_id, False)
        # Three next actions, a review, and the closed unmerged change, which needs a decision.
        assert len(json.loads(subprocess.check_output(command + ['next','--json']))) == 5
        without_home = {k:v for k,v in os.environ.items() if k != 'HOME'}
        assert len(json.loads(subprocess.check_output(command + ['next','--json'], env=without_home))) == 5
        # Closed is not delivered: the demo effort has a merged and a closed unmerged change.
        def demo_effort():
            [status] = json.loads(subprocess.check_output(command + ['status','bootc','--json']))
            return status['effort']['progress'], {i['id']: i for i in status['items']}
        progress, items = demo_effort()
        assert items['demo-1']['outcome'] == 'delivered'
        assert items['demo-6']['outcome'] == 'closed_without_delivery' and items['demo-6']['outcome_reason']
        assert items['demo-6']['status'] == 'Closed'
        assert progress == {'delivered':1,'total':5,'open':3,'needs_attention':1,'abandoned':0}, progress
        text = subprocess.check_output(command + ['status','bootc'], text=True)
        assert '1/5 delivered, 1 needs attention' in text and 'Closed, not delivered' in text, text
        assert 'Closed, not delivered' in subprocess.check_output(command + ['inbox'], text=True)
        # Abandoning applies to synced items; a manual one takes its outcome from its status edit.
        assert call('/api/items/demo-6/abandon', 'POST', {'abandoned':True}, token=token)[0] == 400
        assert subprocess.run(command + ['abandon','demo-6'], capture_output=True).returncode != 0
        subprocess.check_call(command + ['update','demo-6','--status','Closed','--outcome','abandoned'], stdout=subprocess.DEVNULL)
        progress, items = demo_effort()
        assert items['demo-6']['outcome'] == 'abandoned' and items['demo-6']['status'] == 'Closed'
        assert progress == {'delivered':1,'total':4,'open':3,'needs_attention':0,'abandoned':1}, progress
        assert len(json.loads(subprocess.check_output(command + ['next','--json']))) == 4
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
        assert manual['outcome'] == 'open'
        def manual_item():
            snapshot = json.loads(call('/api/snapshot', token=token)[2])
            return snapshot, next(i for i in snapshot['items'] if i['id']==manual['id'])
        # A manual outcome is explicit: status text alone never marks work delivered.
        subprocess.check_call(command + ['update',manual['id'],'--status','Released'], stdout=subprocess.DEVNULL)
        assert manual_item()[1]['outcome'] == 'open'
        subprocess.check_call(command + ['update',manual['id'],'--status','Released','--outcome','delivered'], stdout=subprocess.DEVNULL)
        snapshot, item = manual_item()
        assert item['status'] == 'Released' and item['outcome'] == 'delivered' and item['outcome_reason']
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
        print('PASS: authentication, CSRF, host checks, CLI, outcomes and effort progress, grouping, rollback, source links, manual outcomes, permissions and restart persistence')
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
