#!/usr/bin/env python3
"""Run the GNOME app against a real companion and hub on loopback, without
contacting any live source, and take screenshots of its views.

Run it in a session bus, with an X server it starts itself (Xvfb):

    dbus-run-session -- python3 tests/gnome_smoke.py target/debug/workboard \
        target/debug/workboard-gnome SCREENSHOT_DIR

It checks that the app reads everything the service answers (`--check`, on
the companion and on the hub, with the token the service put in the
session's runtime directory), that it starts and keeps running, that it
announces a new Mine item once, and not again after a restart, also with
its window closed, that it starts without a window with --background, and
that GTK and libadwaita report no warning. The screenshots show each view
in the light style, some in the dark one, and Mine at a phone's width.

The companion and the hub get their work from a demo connector plugin.
The bot's work comes from GitHub, which CI can't reach, so a proxy in front
of the companion adds a canned bot connection to what the app reads; the
`--check` runs read the services directly."""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.request import Request, urlopen

import gi
gi.require_version('Gio', '2.0')
from gi.repository import Gio, GLib  # noqa: E402

workboard = str(Path(sys.argv[1]).resolve())
app = str(Path(sys.argv[2]).resolve())
shots = Path(sys.argv[3]).resolve()
plugin = Path(__file__).resolve().parent / 'plugins' / 'demo-forge'
APP_ID = 'io.github.jmarrero_bot.Workboard'
APP_PATH = '/' + APP_ID.replace('.', '/')
TOKEN_ENV = dict(os.environ, WORKBOARD_DEMO_TOKEN='demo-only-token')
# The window's default size, and the screen's.
WIDTH, HEIGHT = 1100, 760
# The narrowest GNOME's guidelines ask apps to fit.
NARROW = 360
VIEWS = ['mine', 'next', 'inbox', 'efforts', 'connections', 'hub', 'bot']
DARK_VIEWS = ['mine', 'connections', 'bot']


def gdbus(path, method, *args):
    """A gdbus call of the app's `method` at `path`."""
    return ['gdbus', 'call', '--session', '--dest', APP_ID, '--object-path', path, '--method', method, *args]


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def wait_for(what, check, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(.2)
    raise RuntimeError(f'Timed out waiting for {what}')


class Service:
    """A Workboard service with the demo plugin, and its CLI."""

    def __init__(self, root, *serve):
        (root / 'plugins').mkdir(parents=True, mode=0o700)
        root.chmod(0o700)
        shutil.copy(plugin, root / 'plugins' / 'errata')
        # Only the owner may write it, whatever the checkout's umask was.
        (root / 'plugins' / 'errata').chmod(0o700)
        self.root, self.port = root, free_port()
        self.command = [workboard, '--data-dir', str(root), '--port', str(self.port)]
        self.process = subprocess.Popen(self.command + ['serve', *serve], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

        def up():
            if self.process.poll() is not None:
                raise RuntimeError('Service exited: ' + self.process.stderr.read().decode())
            # Any answer of the API: without a token, it refuses. (The page
            # itself is read from web/dist by a debug build, which isn't
            # here.)
            try:
                urlopen(f'http://127.0.0.1:{self.port}/api/snapshot', timeout=2)
                return True
            except HTTPError as error:
                return (root / 'service.token').exists() and error.code == 401
            except OSError as error:
                self.last_error = error
                return False
        try:
            wait_for('the service', up)
        except RuntimeError:
            self.process.terminate()
            raise RuntimeError(f'The service in {root} did not start: token file {(root / "service.token").exists()}, {getattr(self, "last_error", None)!r}, '
                               + self.process.communicate(timeout=10)[1].decode())

    def cli(self, *args, input=None):
        result = subprocess.run(self.command + list(args), env=TOKEN_ENV, input=input, capture_output=True, text=True)
        if result.returncode != 0:
            raise RuntimeError(f'{args}: {result.stderr}')
        return result.stdout

    def sync(self):
        """Sync everything now. The staging connection always fails, so
        this exits unsuccessfully."""
        subprocess.run(self.command + ['sync'], env=TOKEN_ENV, capture_output=True)

    def connect(self, url, label, *extra):
        return self.cli('connect', 'errata', '--url', url, '--label', label, '--token-env', 'WORKBOARD_DEMO_TOKEN', *extra)

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            self.process.wait(timeout=10)


# The canned bot: a connection, items on its board waiting on the person,
# its heartbeat's agent runs, and links that ask it for something.
BOT_ID = 'bot-demo'
BOT_ITEMS = [
    ('31', 'example-org/tracker#31', 'bootc: Support composefs in the install config', 'Needs human', 'P0',
     'Which default should --composefs take? Two options in the question issue.'),
    ('32', 'example-org/bootc#9', 'lint: Warn about /var content in images', 'Draft', 'P1',
     'Ready for review: the fork pull request, tested on a devspace.'),
]


def bot_item(n, key, title, status, priority, why):
    return {
        'id': f'{BOT_ID}:item:{n}', 'connection_id': BOT_ID, 'provider': 'bot', 'remote_id': n,
        'key': key, 'title': title, 'url': f'https://github.com/{key.replace("#", "/issues/")}',
        'status': status, 'outcome': 'open', 'outcome_reason': 'The issue is open', 'action': 'next',
        'updated_at': '2026-10-12T08:00:00Z', 'effort_id': None, 'abandoned_at': None, 'demo': False,
        'zone': 'external', 'reasons': ['bot_waiting'], 'synced_at': None, 'stale': False, 'from_hub': False,
        'bot': {'kind': 'board_item', 'priority': priority, 'why': why, 'news': None, 'branch': None, 'board_item': None},
    }


def bot_answers(now):
    items = [bot_item(*i) for i in BOT_ITEMS]
    for i in items:
        i['synced_at'] = now
    connection = {
        'id': BOT_ID, 'provider': 'bot', 'label': 'Bot', 'base_url': 'https://github.com/', 'account_id': '1',
        'account_name': 'octocat', 'zone': 'external', 'watch_user': None, 'sync_interval': None,
        'sync': {'last_success': now, 'last_attempt': now, 'next_due': None, 'failures': 0, 'error_kind': None, 'error': None, 'held_until': None},
        'schedule': {'interval_secs': 300, 'running': False, 'needs_user': False, 'stale': False}, 'from_hub': False,
    }
    rows = [{
        'id': i['id'], 'provider': 'bot', 'key': i['key'], 'title': i['title'], 'url': i['url'], 'status': i['status'],
        'outcome': 'open', 'outcome_reason': i['outcome_reason'], 'zone': 'external', 'updated_at': i['updated_at'],
        'waits_on': [{'account': 'octocat', 'connection_id': BOT_ID, 'reasons': ['bot_waiting'], 'synced_at': now, 'stale': False}],
        'synced_at': now, 'stale': False, 'from_hub': False,
    } for i in items]
    tracker = 'https://github.com/example-org/tracker/issues/new?labels=triage&body=...'
    run = lambda name, key, status, devspace: {
        'name': name, 'item_url': f'https://github.com/{key.replace("#", "/issues/")}', 'item_key': key,
        'status': status, 'started_at': now, 'devspace': devspace}
    view = {
        'connection_id': BOT_ID, 'label': 'Bot', 'bot_login': 'example-bot',
        'board_url': 'https://github.com/orgs/example-org/projects/1',
        'agents': {
            'connection_id': BOT_ID,
            'heartbeat': {
                'updated_at': now, 'session': 'coordinator', 'loop_state': 'polling', 'next_wake_at': None,
                'runs': [run('wtp-gnome', 'example-org/app#10', 'testing', 'devspace-2'),
                         run('wtp-login', 'example-org/app#12', 'working', None)],
                'skipped': 0, 'comment_url': 'https://github.com/example-org/tracker/issues/1#issuecomment-1', 'stale': False,
            },
            'error': None, 'last_success': now, 'stale': False,
            'board': {'statuses': {'Todo': 6, 'In Progress': 2, 'Draft': 3, 'Needs human': 1, 'Done': 40}, 'fork_prs': 3, 'unreadable': 0},
        },
        'ask': {'issue_url': tracker, 'comment_url': None, 'comment': None},
        'asks': {i['id']: {'issue_url': tracker, 'comment_url': i['url'], 'comment': '@example-bot '} for i in items},
    }
    return connection, items, rows, [view]


class Proxy(BaseHTTPRequestHandler):
    """Forwards to the companion, adding the canned bot to what the app
    reads."""
    port = None

    def do_GET(self):
        self.forward()

    def do_POST(self):
        self.forward()

    def forward(self):
        body = self.rfile.read(int(self.headers.get('Content-Length') or 0))
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ('host', 'connection', 'content-length')}
        request = Request(f'http://127.0.0.1:{self.port}{self.path}', data=body or None, headers=headers, method=self.command)
        try:
            response = urlopen(request, timeout=60)
            status, data = response.status, response.read()
        except HTTPError as error:
            status, data = error.code, error.read()
        if status == 200 and self.command == 'GET':
            now = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
            connection, items, rows, views = bot_answers(now)
            answer = json.loads(data)
            if self.path == '/api/snapshot':
                answer['connections'].append(connection)
                answer['items'].extend(items)
            elif self.path == '/api/mine':
                answer['mine'] = rows + answer['mine']
            elif self.path == '/api/bot':
                answer = views
            data = json.dumps(answer).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


NOTIFICATIONS_XML = '''<node><interface name="org.freedesktop.Notifications">
  <method name="Notify">
    <arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="in"/>
    <arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="as" direction="in"/>
    <arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="out"/>
  </method>
  <method name="CloseNotification"><arg type="u" direction="in"/></method>
  <method name="GetCapabilities"><arg type="as" direction="out"/></method>
  <method name="GetServerInformation">
    <arg type="s" direction="out"/><arg type="s" direction="out"/><arg type="s" direction="out"/><arg type="s" direction="out"/>
  </method>
</interface></node>'''


class Notifications:
    """A notification server on the session bus, recording what it is
    sent: GLib's freedesktop backend sends there."""

    def __init__(self):
        self.received = []
        self.ready = threading.Event()
        threading.Thread(target=self.run, daemon=True).start()
        if not self.ready.wait(10):
            raise RuntimeError('The notification server did not start')

    def run(self):
        context = GLib.MainContext.new()
        context.push_thread_default()
        info = Gio.DBusNodeInfo.new_for_xml(NOTIFICATIONS_XML).interfaces[0]

        def call(connection, sender, path, interface, method, params, invocation):
            if method == 'Notify':
                _, _, _, summary, body, actions, _, _ = params.unpack()
                self.received.append((summary, body, actions))
                invocation.return_value(GLib.Variant('(u)', (len(self.received),)))
            elif method == 'GetCapabilities':
                invocation.return_value(GLib.Variant('(as)', (['body', 'actions'],)))
            elif method == 'GetServerInformation':
                invocation.return_value(GLib.Variant('(ssss)', ('smoke', 'workboard', '1', '1.2')))
            else:
                invocation.return_value(None)

        def acquired(connection, name):
            # PyGObject's override takes Python callables.
            # Deprecated, but the one that takes Python callables.
            connection.register_object('/org/freedesktop/Notifications', info, call, None, None)
            self.ready.set()

        Gio.bus_own_name(Gio.BusType.SESSION, 'org.freedesktop.Notifications', Gio.BusNameOwnerFlags.NONE, None, acquired, None)
        GLib.MainLoop.new(context, False).run()

    def titles(self):
        return [summary for summary, _, _ in self.received]


class App:
    """The GNOME app, driven through its actions on the session bus."""

    def __init__(self, data_dir, port, env, *extra):
        self.log = tempfile.TemporaryFile()
        self.process = subprocess.Popen([app, '--data-dir', str(data_dir), '--port', str(port), *extra], env=env, stdout=self.log, stderr=subprocess.STDOUT)
        try:
            wait_for('the app on the session bus', self.registered)
        except RuntimeError:
            names = subprocess.run(['gdbus', 'call', '--session', '--dest', 'org.freedesktop.DBus', '--object-path', '/org/freedesktop/DBus',
                                    '--method', 'org.freedesktop.DBus.ListNames'], capture_output=True, text=True)
            raise RuntimeError(f'The app never registered: {self.last}\nBus names: {names.stdout}{names.stderr}\nIts output:\n{self.output()}')

    def registered(self):
        self.alive()
        self.last = subprocess.run(gdbus(APP_PATH, 'org.gtk.Actions.List'), capture_output=True, text=True)
        return self.last.returncode == 0

    def alive(self):
        if self.process.poll() is not None:
            raise RuntimeError(f'The app exited with {self.process.returncode}:\n{self.output()}')
        return True

    def action(self, name, parameter=None):
        """Activate one of the app's actions, as `gapplication action`
        does, which would need its desktop file installed."""
        target = f'[<{GLib.Variant("s", parameter).print_(False)}>]' if parameter else '[]'
        subprocess.run(gdbus(APP_PATH, 'org.freedesktop.Application.ActivateAction', name, target, '{}'), check=True, stdout=subprocess.DEVNULL)

    def output(self):
        self.log.seek(0)
        return self.log.read().decode(errors='replace')

    def quit(self):
        self.alive()
        self.action('quit')
        self.process.wait(timeout=10)
        output = self.output()
        print(output, end='')
        # Nothing GTK, GDK or libadwaita warns of, such as content wider
        # than the window, and nothing critical.
        for problem in ['CRITICAL', 'Gtk-WARNING', 'Gdk-WARNING', 'Adwaita-WARNING']:
            assert problem not in output, output
        return output


def screenshot(name, width=WIDTH, height=HEIGHT):
    # The pointer out of the way, where it shows no tooltip.
    subprocess.run(['xdotool', 'mousemove', '0', str(HEIGHT - 1)], check=True)
    time.sleep(1.5)
    subprocess.run(['import', '-window', 'root', '-crop', f'{width}x{height}+0+0', '+repage', str(shots / f'{name}.png')], check=True)


def close(window):
    """Ask `window` to close, as a window manager's close button does."""
    from Xlib import X, display as xdisplay, protocol
    d = xdisplay.Display()
    w = d.create_resource_object('window', int(window))
    protocols, delete = d.intern_atom('WM_PROTOCOLS'), d.intern_atom('WM_DELETE_WINDOW')
    w.send_event(protocol.event.ClientMessage(window=w, client_type=protocols, data=(32, [delete, X.CurrentTime, 0, 0, 0])))
    d.close()


def window_id():
    result = subprocess.run(['xdotool', 'search', '--onlyvisible', '--name', '^Workboard$'], capture_output=True, text=True)
    ids = result.stdout.split()
    return ids[0] if ids else None


with tempfile.TemporaryDirectory(prefix='workboard-gnome-smoke-') as folder:
    folder = Path(folder)
    shots.mkdir(parents=True, exist_ok=True)
    display = os.environ['DISPLAY'] = ':99'
    # The session's runtime directory, where each service also puts its
    # token; inherited by everything below.
    runtime = folder / 'runtime'
    runtime.mkdir(mode=0o700)
    os.environ['XDG_RUNTIME_DIR'] = str(runtime)
    xvfb = subprocess.Popen(['Xvfb', display, '-screen', '0', f'{WIDTH}x{HEIGHT}x24', '-nolisten', 'tcp'])
    services = []
    try:
        wait_for('Xvfb', lambda: Path('/tmp/.X11-unix/X99').exists())
        hub = Service(folder / 'hub', '--hub')
        services.append(hub)
        hub.connect('https://forge.example.org/', 'Partner forge', '--external')
        companion = Service(folder / 'companion', '--demo')
        services.append(companion)
        companion.connect('https://errata.example.invalid/', 'Errata')
        companion.connect('https://staging.errata.example.invalid/', 'Errata staging')
        hub.cli('sync')
        companion.cli('hub', 'set', f'http://127.0.0.1:{hub.port}/', '--token-stdin', input=hub.cli('companion-token'))
        companion.sync()

        # The app reads what each service answers, as it is, with the token
        # in the runtime directory: no data directory here, as in the
        # Flatpak.
        nowhere = folder / 'nowhere'
        for service, expected in [(companion, 'hub set'), (hub, 'hub not set')]:
            checked = subprocess.run([app, '--check', '--data-dir', str(nowhere), '--port', str(service.port)], capture_output=True, text=True)
            print(checked.stdout, checked.stderr, end='')
            assert checked.returncode == 0 and expected in checked.stdout, checked
        assert 'mine: 0' not in subprocess.run([app, '--check', '--data-dir', str(nowhere), '--port', str(companion.port)], capture_output=True, text=True).stdout
        # Without either, it says where it looked.
        missing = subprocess.run([app, '--check', '--data-dir', str(nowhere), '--port', str(free_port())], capture_output=True, text=True)
        assert missing.returncode != 0 and 'start `workboard serve`' in missing.stderr, missing

        Proxy.port = companion.port
        proxy = ThreadingHTTPServer(('127.0.0.1', 0), Proxy)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        notifications = Notifications()

        config = folder / 'config'
        (config / 'gtk-4.0').mkdir(parents=True)
        (config / 'gtk-4.0' / 'settings.ini').write_text('[Settings]\ngtk-font-name=Adwaita Sans 11\n')
        env = dict(os.environ, GDK_BACKEND='x11', GSK_RENDERER='cairo', ADW_DISABLE_PORTAL='1',
                   GNOTIFICATION_BACKEND='freedesktop', XDG_STATE_HOME=str(folder / 'state'), XDG_CONFIG_HOME=str(config),
                   NO_AT_BRIDGE='1', GTK_A11Y='none')
        notified = folder / 'state' / 'workboard-gnome' / 'notified.json'

        # The first run records what waits, and announces none of it.
        first = App(companion.root, proxy.server_address[1], env)
        wait_for('the first answer', notified.exists)
        time.sleep(1)
        assert notifications.received == [], notifications.received
        for view in VIEWS:
            first.action('show-view', view)
            screenshot(f'{view}')
        # At a phone's width, the sidebar and the view take turns.
        window = wait_for('the window', window_id)
        subprocess.run(['xdotool', 'windowsize', window, str(NARROW), str(HEIGHT)], check=True)
        first.action('show-view', 'mine')
        screenshot('narrow-mine', NARROW)

        # New work is announced once.
        companion.connect('https://new.errata.example.invalid/', 'New errata')
        companion.sync()
        first.action('refresh')
        wait_for('a notification', lambda: notifications.received)
        time.sleep(2)
        assert notifications.titles() == ['Assigned: skopeo security update'], notifications.received
        first.action('refresh')
        time.sleep(3)
        assert len(notifications.received) == 1, notifications.received
        first.quit()

        # Not again after a restart; and the dark style.
        dark = App(companion.root, proxy.server_address[1], dict(env, ADW_DEBUG_COLOR_SCHEME='prefer-dark'))
        dark.action('refresh')
        time.sleep(3)
        assert len(notifications.received) == 1, notifications.received
        for view in DARK_VIEWS:
            dark.action('show-view', view)
            screenshot(f'dark-{view}')
        # Closing the window keeps it running, and announcing new work, on
        # its own schedule: no refresh is asked for.
        dark.action('show-view', 'mine')
        # Asked again until it hides: an X client message sent while the
        # window is still busy redrawing can go unnoticed.
        def closed():
            window = window_id()
            if window is None:
                return True
            close(window)
            time.sleep(1)
            return False
        wait_for('the window to hide', closed, timeout=30)
        companion.connect('https://later.errata.example.invalid/', 'Later errata')
        companion.sync()
        wait_for('a notification with the window closed', lambda: len(notifications.received) > 1, timeout=75)
        assert notifications.titles()[1:] == ['Review requested: podman security update'], notifications.received
        assert window_id() is None
        dark.quit()

        # Started in the background, as at login: no window, but running.
        background = App(companion.root, proxy.server_address[1], env, '--background')
        time.sleep(3)
        background.alive()
        assert window_id() is None
        background.quit()
        print(f'Screenshots in {shots}: {", ".join(sorted(p.name for p in shots.iterdir()))}')
    finally:
        for service in services:
            service.stop()
        xvfb.terminate()
