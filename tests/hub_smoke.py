#!/usr/bin/env python3
"""Run a hub and a companion as two processes on loopback, without
contacting any live source. The hub syncs external work from a connector
plugin; the companion has internal work of its own and pulls the hub's
through a logging proxy. The companion must show both, while nothing of
its internal work ever reaches the hub: not in what it sends, not in any
hub answer, not in the hub's data directory."""
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/workboard').resolve())
plugin = Path(__file__).resolve().parent / 'plugins' / 'fake-forge'
TAILNET = 'hub.example.ts.net'
# Everything internal on the companion carries this, in some case: the
# plugin marks the work of an instance named after it (its keys, titles,
# links, statuses and account), and the error of one that is down. Its
# credential is marked too, unlike the hub's.
LABEL = 'marked-' + secrets.token_hex(4)
DOWN = LABEL + '-down'
MARKERS = [LABEL.encode()]
TOKEN_ENV = dict(os.environ, WORKBOARD_SMOKE_TOKEN='test-only-token', WORKBOARD_LABEL_TOKEN='test-only-' + LABEL, WORKBOARD_DOWN_TOKEN='test-only-' + DOWN)


def leaks(data):
    """Whether `data` carries any of the companion's internal work."""
    data = data.lower() if isinstance(data, bytes) else data.encode().lower()
    return any(marker in data for marker in MARKERS)


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def call(port, path, method='GET', body=None, token=None, cookie=None, origin=None, host=None, extra=None):
    headers = dict(extra or {})
    if token is not None: headers['Authorization'] = 'Bearer ' + token
    if cookie is not None: headers['Cookie'] = cookie
    if origin is not None: headers['Origin'] = origin
    if host is not None: headers['Host'] = host
    data = None
    if body is not None:
        headers['Content-Type'] = 'application/json'
        data = json.dumps(body).encode()
    request = Request(f'http://127.0.0.1:{port}{path}', data=data, headers=headers, method=method)
    try:
        response = urlopen(request, timeout=10)
        return response.status, response.headers, response.read()
    except HTTPError as error:
        return error.code, error.headers, error.read()


class Service:
    """A Workboard service, and its CLI."""

    def __init__(self, root, *serve):
        self.root, self.port = root, free_port()
        self.command = [binary, '--data-dir', str(root), '--port', str(self.port)]
        self.process = subprocess.Popen(self.command + ['serve', *serve], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        for _ in range(200):
            if self.process.poll() is not None:
                raise RuntimeError('Service exited: ' + self.process.stderr.read().decode())
            if (root / 'service.token').exists():
                try:
                    if call(self.port, '/')[0] == 200: break
                except OSError: pass
            time.sleep(.05)
        else: raise RuntimeError('Service did not start')
        self.token = (root / 'service.token').read_text().strip()

    def call(self, path, method='GET', body=None, **headers):
        headers.setdefault('token', self.token)
        return call(self.port, path, method, body, **headers)

    def json(self, path):
        status, _, body = self.call(path)
        assert status == 200, (path, status, body)
        return json.loads(body)

    def cli(self, *args, check=True, input=None):
        result = subprocess.run(self.command + list(args), env=TOKEN_ENV, input=input, capture_output=True, text=True)
        if check and result.returncode != 0:
            raise RuntimeError(f'{args}: {result.stderr}')
        return result

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            self.process.wait(timeout=5)


def refused(command, needle):
    """Run a service that must refuse to start, and say why."""
    result = subprocess.run(command, capture_output=True, text=True, timeout=20)
    assert result.returncode != 0 and needle in result.stderr, result


def install_plugin(root):
    (root / 'plugins').mkdir(parents=True, mode=0o700)
    root.chmod(0o700)
    shutil.copy(plugin, root / 'plugins' / 'errata')


class Proxy(BaseHTTPRequestHandler):
    """Forwards to the hub, and logs everything that passes, both ways."""
    hub_port = None
    sent = []
    answers = []

    def forward(self):
        body = self.rfile.read(int(self.headers.get('Content-Length') or 0))
        self.sent.append((self.command, self.path, dict(self.headers), body))
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ('host', 'connection', 'content-length')}
        request = Request(f'http://127.0.0.1:{self.hub_port}{self.path}', data=body or None, headers=headers, method=self.command)
        try:
            response = urlopen(request, timeout=10)
            status, answer, data = response.status, response.headers, response.read()
        except HTTPError as error:
            status, answer, data = error.code, error.headers, error.read()
        except URLError:
            status, answer, data = 502, {}, b'{"error":"The hub is down"}'
        self.answers.append(data)
        self.send_response(status)
        for k, v in dict(answer).items():
            if k.lower() not in ('connection', 'transfer-encoding', 'content-length'):
                self.send_header(k, v)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    do_GET = do_HEAD = do_POST = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = forward

    def log_message(self, *args):
        pass


with tempfile.TemporaryDirectory(prefix='workboard-hub-smoke-') as folder:
    hub_root, companion_root = Path(folder) / 'hub', Path(folder) / 'companion'
    for root in (hub_root, companion_root):
        install_plugin(root)
    services = []
    try:
        # A hub never takes the demo's internal work, and only a hub takes
        # another name.
        other = [binary, '--data-dir', str(Path(folder) / 'other'), '--port', str(free_port()), 'serve']
        refused(other + ['--hub', '--demo'], 'cannot be used with')
        refused(other + ['--allowed-host', TAILNET], '--hub')

        hub = Service(hub_root, '--hub', '--allowed-host', TAILNET)
        services.append(hub)
        hub_answers = []

        def hub_call(path, method='GET', body=None, **headers):
            status, response_headers, data = hub.call(path, method, body, **headers)
            hub_answers.append(data)
            return status, response_headers, data

        # The hub connects external sources only, and keeps no manual links.
        connected = hub.cli('connect', 'errata', '--url', 'https://forge.example/', '--label', 'Forge', '--external', '--token-env', 'WORKBOARD_SMOKE_TOKEN')
        assert 'external' in connected.stdout, connected
        internal = hub.cli('connect', 'errata', '--url', 'https://errata.example.invalid/', '--token-env', 'WORKBOARD_SMOKE_TOKEN', check=False)
        assert internal.returncode != 0 and 'external sources only' in internal.stderr, internal
        manual = hub.cli('add', '--provider', 'errata', '--key', 'E-1', '--title', 'Advisory', '--url', 'https://errata.example.invalid/1', check=False)
        assert manual.returncode != 0 and 'hub' in manual.stderr, manual
        hub.cli('sync')
        board = json.loads(hub_call('/api/snapshot')[2])
        assert board['mode'] == 'hub' and board['hub'] is None, board
        assert [c['zone'] for c in board['connections']] == ['external'], board
        hub_items = sorted(i['id'] for i in board['items'])
        assert len(hub_items) == 2 and all(i['zone'] == 'external' for i in board['items']), board
        status, _, body = hub_call('/api/efforts', 'POST', {'title': 'Hub effort', 'item_ids': hub_items[:1], 'share_title': True})
        assert status == 200, body

        # Reached by its tailnet name through an HTTPS front: that name is
        # accepted, its session cookie is Secure, and a browser's changes
        # need that name's HTTPS origin.
        assert hub_call('/api/snapshot', host=TAILNET)[0] == 200
        assert hub_call('/api/snapshot', host='other.example.ts.net')[0] == 403
        status, headers, _ = hub_call('/api/session', 'POST', host=TAILNET)
        assert status == 200 and 'Secure' in headers['Set-Cookie'] and 'HttpOnly' in headers['Set-Cookie'], headers
        cookie = headers['Set-Cookie'].split(';')[0]
        assert 'Secure' not in hub_call('/api/session', 'POST')[1]['Set-Cookie']
        via = dict(token=None, cookie=cookie, host=TAILNET)
        assert hub_call('/api/efforts', 'POST', {'title': 'Via the tailnet'}, origin=f'https://{TAILNET}', **via)[0] == 200
        assert hub_call('/api/efforts', 'POST', {'title': 'Cross'}, origin=f'http://127.0.0.1:{hub.port}', **via)[0] == 403
        assert hub_call('/api/efforts', 'POST', {'title': 'Cross'}, origin=f'http://{TAILNET}', **via)[0] == 403
        # Never through Tailscale Funnel, which marks what it brings from
        # the internet: not the API, not the page.
        funnel = {'Tailscale-Funnel-Request': '?1'}
        assert hub_call('/api/snapshot', host=TAILNET, extra=funnel)[0] == 403
        assert hub_call('/', token=None, host=TAILNET, extra=funnel)[0] == 403
        assert hub_call('/', token=None, host=TAILNET)[0] == 200

        # The companion token reads the export, and nothing else.
        companion_token = hub.cli('companion-token').stdout.strip()
        assert companion_token and companion_token != hub.token
        assert hub_call('/api/export/external', token=companion_token)[0] == 200
        assert hub_call('/api/snapshot', token=companion_token)[0] == 403
        assert hub_call('/api/efforts', 'POST', {'title': 'No'}, token=companion_token)[0] == 403
        assert hub_call('/api/companion-token', token=companion_token)[0] == 403

        # The companion reaches the hub through a proxy that logs it all.
        Proxy.hub_port = hub.port
        proxy = ThreadingHTTPServer(('127.0.0.1', 0), Proxy)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        hub_url = f'http://127.0.0.1:{proxy.server_address[1]}/'

        companion = Service(companion_root)
        services.append(companion)
        # Its internal work, all of it marked, and a connection whose syncs
        # fail with a marked error.
        companion.cli('connect', 'errata', '--url', f'https://{LABEL}.example/', '--label', f'{LABEL} Errata', '--token-env', 'WORKBOARD_LABEL_TOKEN')
        companion.cli('connect', 'errata', '--url', f'https://{DOWN}.example/', '--token-env', 'WORKBOARD_DOWN_TOKEN')
        companion.cli('add', '--provider', 'errata', '--key', f'{LABEL}-advisory', '--title', f'{LABEL} advisory', '--url', f'https://{LABEL}.example/advisory/1')
        assert DOWN + ' is down' in companion.cli('sync', check=False).stdout
        own = companion.json('/api/snapshot')
        assert own['mode'] == 'local' and own['hub'] is None, own
        assert all(leaks(json.dumps(c)) for c in own['connections']), own['connections']
        internal_items = [i['id'] for i in own['items']]
        assert len(internal_items) == 3 and all(leaks(json.dumps(i)) for i in own['items']), own
        status, _, body = companion.call('/api/efforts', 'POST', {'title': f'{LABEL} effort', 'note': f'{LABEL} note', 'item_ids': internal_items, 'share_title': True})
        assert status == 200, body

        # A hub address the token would cross a network to in the clear,
        # and a token the hub refuses, are never kept.
        plain = companion.cli('hub', 'set', 'http://hub.example.ts.net/', '--token-stdin', input=companion_token, check=False)
        assert plain.returncode != 0 and 'HTTPS' in plain.stderr, plain
        wrong = companion.cli('hub', 'set', hub_url, '--token-stdin', input='wrong-token', check=False)
        assert wrong.returncode != 0 and 'refused the token' in wrong.stderr, wrong
        assert companion.json('/api/hub') is None
        assert 'Pulling from' in companion.cli('hub', 'set', hub_url, '--token-stdin', input=companion_token).stdout

        # The companion's board shows both, the hub's marked and read-only.
        board = companion.json('/api/snapshot')
        assert board['hub']['url'] == hub_url and not board['hub']['schedule']['stale'], board['hub']
        from_hub = [i for i in board['items'] if i['from_hub']]
        assert sorted(i['id'] for i in from_hub) == sorted('hub:' + i for i in hub_items), board
        assert all(i['zone'] == 'external' and not i['stale'] for i in from_hub), from_hub
        assert {i['id'] for i in board['items'] if not i['from_hub']} == set(internal_items), board
        assert [c['from_hub'] for c in board['connections']] == [False, False, True], board['connections']
        # Only efforts with external work leave the hub.
        assert [e['title'] for e in board['efforts'] if e['from_hub']] == ['Hub effort'], board['efforts']
        inbox = json.loads(companion.cli('inbox', '--json').stdout)
        assert {i['from_hub'] for i in inbox} == {True, False}, inbox
        mine = json.loads(companion.cli('mine', '--json').stdout)
        rows = {r['key']: r for r in mine['mine']}
        assert rows['FF-1']['from_hub'] and rows[f'{LABEL.upper()}-1']['zone'] == 'internal', rows
        assert 'from the hub' in companion.cli('inbox').stdout
        assert 'Hub  ' + hub_url in companion.cli('connections').stdout
        # Read-only: nothing of the hub's is changed here.
        hub_item = from_hub[0]['id']
        assert companion.call('/api/links', 'POST', {'item': hub_item, 'effort': None})[0] == 400
        assert companion.call(f'/api/items/{hub_item}/abandon', 'POST', {'abandoned': True, 'force': True})[0] == 400
        # The companion's own export carries its own external work only:
        # nothing internal, and nothing it pulled.
        exported = companion.cli('export', '--external').stdout
        assert json.loads(exported)['items'] == [] and not leaks(exported), exported

        # An account the companion connects too shows once, its own copy.
        dup = companion.cli('connect', 'errata', '--url', 'https://forge.example/', '--label', 'Forge here', '--external', '--token-env', 'WORKBOARD_SMOKE_TOKEN')
        dup_id = dup.stdout.split('(')[1].split(',')[0]
        companion.cli('sync', dup_id)
        board = companion.json('/api/snapshot')
        assert not any(c['from_hub'] for c in board['connections']), board['connections']
        assert not any(i['from_hub'] for i in board['items']), board['items']
        assert len([i for i in board['items'] if i['connection_id'] == dup_id]) == 2, board['items']
        companion.cli('disconnect', dup_id)
        assert any(i['from_hub'] for i in companion.json('/api/snapshot')['items'])

        # An explicit sync pulls too, while the connection that is down
        # fails on its own.
        companion.cli('sync', check=False)

        # The companion sent the hub GETs of its export, and nothing else,
        # and nothing of its internal work.
        assert len(Proxy.sent) >= 3, Proxy.sent
        for method, path, headers, body in Proxy.sent:
            assert (method, path, body) == ('GET', '/api/export/external', b''), (method, path, body)
            assert not leaks(json.dumps(headers)), headers
        # No hub answer, to the companion or to anyone, carries it.
        for answer in Proxy.answers + hub_answers + [hub_call('/api/snapshot')[2], hub_call('/api/mine')[2], hub_call('/api/export/external')[2]]:
            assert not leaks(answer), answer
        # Nor does anything the hub stored.
        for path in hub_root.rglob('*'):
            if path.is_file():
                assert not leaks(path.read_bytes()), path

        # With the hub down, its work on the companion is stale, never
        # current, and the companion's own work is unaffected.
        hub.stop()
        down = companion.cli('sync', check=False)
        assert down.returncode != 0, down
        board = companion.json('/api/snapshot')
        assert board['hub']['schedule']['stale'] and board['hub']['sync']['error'], board['hub']
        from_hub = [i for i in board['items'] if i['from_hub']]
        assert from_hub and all(i['stale'] for i in from_hub), from_hub
        assert all(c['schedule']['stale'] for c in board['connections'] if c['from_hub'])
        assert all(not i['stale'] for i in board['items'] if not i['from_hub']), board['items']
        mine = json.loads(companion.cli('mine', '--json').stdout)
        assert all(r['stale'] for r in mine['mine'] if r['from_hub']), mine
        assert 'Stale' in companion.cli('hub', 'show').stdout

        # A hub's data directory is never served in normal mode, nor a
        # companion's as a hub.
        refused(hub.command + ['serve'], "hub's")
        refused([binary, '--data-dir', str(companion_root), '--port', str(free_port()), 'serve', '--hub'], "isn't a hub's")
        hub = Service(hub_root, '--hub')
        services.append(hub)
        assert hub.cli('companion-token').stdout.strip() == companion_token, 'kept across restarts'
        rotated = hub.cli('companion-token', '--rotate').stdout.strip()
        assert rotated != companion_token
        assert hub.call('/api/export/external', token=companion_token)[0] == 401
        assert hub.call('/api/export/external', token=rotated)[0] == 200
        proxy.shutdown()
        print('PASS: hub mode refusals, tailnet name and origins, the companion token, a companion pulling read-only GETs, the merged board and Mine view, duplicates, read-only hub work, no internal work at the hub, staleness with the hub down, and mode persistence')
    finally:
        for service in services:
            service.stop()
