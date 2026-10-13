#!/usr/bin/env python3
"""Exercise the actual Rust service and CLI without contacting any live sources."""
import hashlib
import json
import os
from pathlib import Path
import shutil
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
    runtime = Path(folder) / 'runtime'
    runtime.mkdir(mode=0o700)
    process = subprocess.Popen(command + ['serve', '--demo'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                               env=dict(os.environ, XDG_RUNTIME_DIR=str(runtime)))
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
        # A copy in the session's runtime directory, by port, for sandboxed
        # clients, as private as the data directory's.
        # One service, or credentials migration, per data directory.
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            other_port = sock.getsockname()[1]
        second = subprocess.run([binary, '--data-dir', str(root), '--port', str(other_port), 'serve'], capture_output=True, text=True, timeout=20)
        assert second.returncode != 0 and 'Another Workboard service' in second.stderr, second
        migrating = subprocess.run(command + ['credentials', 'migrate', '--to', 'files'], capture_output=True, text=True)
        assert migrating.returncode != 0 and 'Another Workboard service' in migrating.stderr, migrating
        published = runtime / 'workboard' / f'service-{port}.token'
        assert published.read_text().strip() == token
        assert published.stat().st_mode & 0o777 == 0o600 and published.parent.stat().st_mode & 0o777 == 0o700
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
        assert json.loads(exported) == {'zone':'external','connections':[],'items':[],'efforts':[],'bots':[]}, exported
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
        # A connector plugin with an internal and an external connection:
        # only the external one's work leaves, even from an effort that mixes them.
        plugins = root / 'plugins'
        plugins.mkdir(mode=0o700)
        shutil.copy(Path(__file__).resolve().parent / 'plugins' / 'fake-forge', plugins / 'errata')
        plugin_env = dict(os.environ, WORKBOARD_SMOKE_TOKEN='test-only-token')
        def connect(url, *extra):
            return subprocess.check_output(command + ['connect','errata','--url',url,'--label',url,'--token-env','WORKBOARD_SMOKE_TOKEN',*extra], env=plugin_env, text=True)
        connected = connect('https://errata.example.invalid/')
        # It says which plugin file it trusts, as sha256sum would.
        assert 'internal' in connected and hashlib.sha256((plugins / 'errata').read_bytes()).hexdigest() in connected, connected
        assert 'external' in connect('https://forge.example/', '--external')
        # A plugin that echoes the credential as the account name isn't connected.
        echo = subprocess.run(command + ['connect','errata','--url','https://echo-account.example/','--token-env','WORKBOARD_SMOKE_TOKEN'], env=plugin_env, capture_output=True, text=True)
        assert echo.returncode != 0 and 'test-only-token' not in echo.stdout + echo.stderr, echo
        # A plugin gets only the proxy, CA bundle and time zone variables of
        # the service's environment, and only a plugin gets any.
        for provider, name, why in [
            ('errata', 'LD_PRELOAD', 'It may get only HTTP_PROXY'),
            ('errata', 'GH_TOKEN', 'It may get only HTTP_PROXY'),
            ('errata', 'HOME', 'private, empty home'),
            ('github', 'HTTPS_PROXY', 'connector plugin'),
        ]:
            refused = subprocess.run(command + ['connect',provider,'--url','https://env.example/','--token-env','WORKBOARD_SMOKE_TOKEN','--plugin-env',name], env=plugin_env, capture_output=True, text=True)
            assert refused.returncode != 0 and why in refused.stderr, (name, refused)
        subprocess.check_call(command + ['sync'], stdout=subprocess.DEVNULL)
        body = call('/api/snapshot', token=token)[2]
        assert b'test-only-token' not in body
        synced = [i for i in json.loads(body)['items'] if i['connection_id']]
        assert len(synced) == 4 and {i['zone'] for i in synced} == {'internal','external'}, synced
        # Freshness: each connection's sync state and schedule, and each
        # item as fresh as its connection's last success.
        snapshot = json.loads(body)
        for c in snapshot['connections']:
            assert c['sync']['last_success'] and c['sync']['next_due'] and c['sync']['error'] is None, c
            assert c['schedule']['interval_secs'] == 300 and not c['schedule']['needs_user'] and not c['schedule']['stale'], c
        last_success = {c['id']: c['sync']['last_success'] for c in snapshot['connections']}
        assert all(i['synced_at'] == last_success[i['connection_id']] and not i['stale'] for i in synced), synced
        assert all(i['synced_at'] is None and not i['stale'] for i in snapshot['items'] if not i['connection_id'])
        assert 'Next sync in ' in subprocess.check_output(command + ['connections'], text=True)
        internal_item = next(i['id'] for i in synced if i['zone'] == 'internal')
        external_items = sorted(i['id'] for i in synced if i['zone'] == 'external')
        mixed = {'title':'Mixed zones','note':'internal-only note','item_ids':[internal_item, external_items[0]]}
        status, _, body = call('/api/efforts', 'POST', mixed, token=token)
        assert status == 200
        mixed_id = json.loads(body)['id']
        exported = subprocess.check_output(command + ['export','--external'], text=True)
        assert 'errata.example.invalid' not in exported and internal_item not in exported, exported
        assert 'internal-only note' not in exported and 'test-only-token' not in exported, exported
        assert 'Mixed zones' not in exported, exported
        exported = json.loads(exported)
        assert [c['base_url'] for c in exported['connections']] == ['https://forge.example/'], exported
        assert sorted(i['id'] for i in exported['items']) == external_items, exported
        assert sorted(tuple(i['reasons']) for i in exported['items']) == [('assigned', 'mentioned'), ('review_requested',)], exported
        # The Mine view: both connections report both changes, so each shows
        # once, with all its reasons, and internal because one of them is.
        assert call('/api/mine')[0] == 401
        mine = json.loads(subprocess.check_output(command + ['mine', '--json']))
        assert mine['watching'] == [] and mine['watch_errors'] == [], mine
        rows = {r['key']: r for r in mine['mine']}
        assert sorted(rows) == ['FF-1', 'FF-2'], mine
        # Each connection's account with its own reasons.
        assert [w['reasons'] for w in rows['FF-2']['waits_on']] == [['assigned', 'mentioned']] * 2, rows
        assert [w['reasons'] for w in rows['FF-1']['waits_on']] == [['review_requested']] * 2, rows
        assert rows['FF-1']['status'] == 'open', rows
        assert all(len({w['connection_id'] for w in r['waits_on']}) == 2 and r['zone'] == 'internal' for r in rows.values()), rows
        text = subprocess.check_output(command + ['mine'], text=True)
        assert 'Waiting on you (2)' in text and 'fixture: assigned, mentioned' in text and 'Watching' not in text, text
        # Only a GitHub connection can watch another user.
        forge = next(c['id'] for c in exported['connections'])
        watch = subprocess.run(command + ['watch', forge, 'someone'], capture_output=True, text=True)
        assert watch.returncode != 0 and 'Only a GitHub connection' in watch.stderr, watch
        # A bot connection is checked whole before its token goes anywhere:
        # it takes its board, tracker, heartbeat, login and forge
        # organization, and the external zone, and nothing else takes them.
        bot_args = ['--board','orgs/o/1','--tracker','o/tracker','--heartbeat','1','--bot-login','some-bot','--forge-org','o']
        def connect_bot(provider, *extra):
            return subprocess.run(command + ['connect',provider,'--url','https://github.example.invalid/','--token-env','WORKBOARD_SMOKE_TOKEN',*extra], env=plugin_env, capture_output=True, text=True)
        for provider, extra, needle in [
            ('bot', [], 'board'),
            ('bot', ['--external','--board','orgs/o/1'], '--tracker'),
            ('bot', bot_args, 'external'),
            ('bot', ['--external',*bot_args[:-2],'--forge-org','a/b'], 'organization'),
            ('errata', bot_args, 'Only a bot connection'),
        ]:
            refused = connect_bot(provider, *extra)
            assert refused.returncode != 0 and needle in refused.stderr, (extra, refused)
        # Without a bot, there is nothing to show or ask.
        assert 'No bot is connected' in subprocess.check_output(command + ['bot'], text=True)
        assert json.loads(subprocess.check_output(command + ['bot','--json'])) == []
        ask = subprocess.run(command + ['ask','--print'], capture_output=True, text=True)
        assert ask.returncode != 0 and 'No bot is connected' in ask.stderr, ask
        [effort_out] = exported['efforts']
        assert effort_out['title'] == 'Effort ' + mixed_id.replace('-', '')[:8], effort_out
        assert effort_out['progress']['total'] == 1, effort_out
        # Once shared, the title goes too.
        subprocess.check_call(command + ['share-title', mixed_id], stdout=subprocess.DEVNULL)
        [effort_out] = json.loads(subprocess.check_output(command + ['export','--external']))['efforts']
        assert effort_out['title'] == 'Mixed zones', effort_out
        # Scheduled sync: connections that verify but then fail sync on
        # their own, with nobody asking, and say how.
        connect('https://list-flaky.example/', '--external', '--sync-interval', '10m')
        connect('https://list-denied.example/')
        def failing():
            connections = json.loads(call('/api/snapshot', token=token)[2])['connections']
            return {c['base_url']: c for c in connections if c['base_url'].startswith('https://list-')}
        for _ in range(200):
            if all(c['sync']['last_attempt'] for c in failing().values()): break
            time.sleep(.05)
        else: raise RuntimeError('No scheduled sync ran: ' + json.dumps(failing()))
        flaky, denied = failing()['https://list-flaky.example/'], failing()['https://list-denied.example/']
        # A transient failure backs off, and its data isn't current.
        assert flaky['sync']['error_kind'] == 'transient' and flaky['sync']['failures'] >= 1, flaky
        assert flaky['sync']['next_due'] and flaky['sync']['last_success'] is None, flaky
        assert flaky['schedule']['interval_secs'] == 600 and flaky['schedule']['stale'], flaky
        # A rejected credential stops scheduled syncs until it is verified.
        assert denied['sync']['error_kind'] == 'auth' and denied['sync']['next_due'] is None, denied
        assert denied['schedule']['needs_user'], denied
        text = subprocess.check_output(command + ['connections'], text=True)
        assert 'backing off' in text and 'Needs you' in text and f"workboard verify {denied['id']}" in text, text
        # Explicit syncs still run, of one connection, and fail.
        result = subprocess.run(command + ['sync', flaky['id']], capture_output=True, text=True)
        assert result.returncode != 0, result
        [report] = json.loads(result.stdout)['results']
        assert report['trigger'] == 'explicit' and report['error_kind'] == 'transient', report
        subprocess.check_call(command + ['schedule', flaky['id'], '15m'], stdout=subprocess.DEVNULL)
        assert failing()['https://list-flaky.example/']['schedule']['interval_secs'] == 900
        # A change is checked whole before any of it is saved.
        patch = {'sync_interval': 1200, 'watch_user': 'someone'}
        assert call(f"/api/connections/{flaky['id']}", 'PATCH', patch, token=token)[0] == 400
        assert failing()['https://list-flaky.example/']['schedule']['interval_secs'] == 900
        assert subprocess.run(command + ['schedule', flaky['id'], '10s'], capture_output=True).returncode != 0
        # The export carries each connection's freshness and error kind,
        # never the message.
        exported = subprocess.check_output(command + ['export','--external'], text=True)
        assert 'Try later' not in exported, exported
        out = {c['base_url']: c for c in json.loads(exported)['connections']}
        assert out['https://forge.example/']['stale'] is False and out['https://forge.example/']['last_success'], out
        assert out['https://list-flaky.example/']['stale'] is True and out['https://list-flaky.example/']['error_kind'] == 'transient', out
        assert out['https://list-flaky.example/']['last_success'] is None, out
        assert 'https://list-denied.example/' not in out, out
        # Verifying the credential again succeeds, and brings the schedule back.
        verified = subprocess.check_output(command + ['verify', denied['id']], text=True)
        assert 'syncs again soon' in verified, verified
        status, _, body = call(f"/api/connections/{denied['id']}/verify", 'POST', {}, token=token)
        assert status == 200, body
        back = json.loads(body)
        assert not back['schedule']['needs_user'] and back['sync']['error'] is None, back
        assert back['sync']['next_due'] and back['sync']['failures'] == 0, back
        # Its schedule is back: it syncs again unasked, and stops again,
        # since the listing still refuses it.
        for _ in range(200):
            if failing()['https://list-denied.example/']['sync']['error_kind'] == 'auth': break
            time.sleep(.05)
        else: raise RuntimeError('No scheduled sync after verify: ' + json.dumps(failing()))
        assert failing()['https://list-denied.example/']['schedule']['needs_user']
        # A plugin runs only while it is the file its connection trusts:
        # once it changes, the connection stops until the person trusts the
        # new file, which they see by its hash first.
        forge = next(c for c in json.loads(call('/api/snapshot', token=token)[2])['connections'] if c['base_url'] == 'https://forge.example/')
        trusted = hashlib.sha256((plugins / 'errata').read_bytes()).hexdigest()
        assert forge['plugin'] == {'sha256': trusted}, forge
        with open(plugins / 'errata', 'a') as script: script.write('\n# Changed after it was trusted.\n')
        changed = hashlib.sha256((plugins / 'errata').read_bytes()).hexdigest()
        result = subprocess.run(command + ['sync', forge['id']], capture_output=True, text=True)
        assert result.returncode != 0, result
        [report] = json.loads(result.stdout)['results']
        assert report['error_kind'] == 'auth' and changed in report['error'] and trusted in report['error'], report
        assert f"workboard plugin trust {forge['id']}" in subprocess.check_output(command + ['connections'], text=True)
        shown = subprocess.run(command + ['plugin', 'trust', forge['id']], capture_output=True, text=True, stdin=subprocess.DEVNULL)
        assert shown.returncode != 0 and trusted in shown.stdout and changed in shown.stdout, shown
        assert f'--sha256 {changed}' in shown.stderr, shown
        stale = subprocess.run(command + ['plugin', 'trust', forge['id'], '--sha256', trusted], capture_output=True, text=True)
        assert stale.returncode != 0 and 'changed again' in stale.stderr, stale
        assert changed in subprocess.check_output(command + ['plugin', 'trust', forge['id'], '--sha256', changed], text=True)
        subprocess.check_call(command + ['sync', forge['id']], stdout=subprocess.DEVNULL)
        assert 'trusts the file there now' in subprocess.check_output(command + ['plugin', 'trust', forge['id']], text=True)
        # Which variables a plugin gets is the connection's setting.
        allowed = subprocess.check_output(command + ['plugin', 'env', forge['id'], 'NO_PROXY', 'HTTPS_PROXY'], text=True)
        assert 'gets HTTPS_PROXY, NO_PROXY from' in allowed, allowed
        assert 'gets HTTPS_PROXY, NO_PROXY from' in subprocess.check_output(command + ['plugin', 'env', forge['id']], text=True)
        assert call(f"/api/connections/{forge['id']}", 'PATCH', {'plugin_env': ['LD_PRELOAD']}, token=token)[0] == 400
        assert 'nothing from' in subprocess.check_output(command + ['plugin', 'env', forge['id'], '--clear'], text=True)
        states = {c['id']: c['sync'] for c in json.loads(call('/api/snapshot', token=token)[2])['connections']}
        if os.name == 'posix':
            assert (root.stat().st_mode & 0o777) == 0o700
            assert ((root / 'service.token').stat().st_mode & 0o777) == 0o600
            # SQLite's write-ahead log and shared memory too, which hold
            # recent rows while the service runs.
            for name in ['work.sqlite', 'work.sqlite-wal', 'work.sqlite-shm']:
                assert ((root / name).stat().st_mode & 0o777) == 0o600, name
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
        # Sync state is kept: the last successes are those from before.
        for c in snapshot['connections']:
            if c['id'] in last_success:
                assert c['sync']['last_success'] == states[c['id']]['last_success'], (c, states)
        print('PASS: authentication, CSRF, host checks, CLI, outcomes and effort progress, grouping, rollback, source links, manual outcomes, a connector plugin and its pinning, the external export, the Mine view, scheduled sync and freshness, permissions and restart persistence')
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
