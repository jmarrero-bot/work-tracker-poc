#!/usr/bin/env python3
"""Run the hub's container image as it is deployed, check its health and
its API, and stop it. The data directory is a new named volume, so the
image must make it the service's own; a restart must keep the hub's
companion token, so the volume holds its state.

    image_smoke.py IMAGE [--engine docker|podman] [--network host|publish]

The service listens on the container's loopback only. `--network publish`
(the default) publishes its port to the host's loopback, as the quadlet in
atomic-setup does, with rootless podman's pasta told to forward host
loopback connections to the container's loopback (by default it forwards
them to the container's own address, where nothing listens). Docker can't,
so under docker use `--network host`."""
import argparse
import io
import json
import secrets
import socket
import subprocess
import tarfile
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

PORT = 8421
LOCAL = f'127.0.0.1:{PORT}'
TAILNET = 'hub.example.ts.net'
DATA = '/var/lib/workboard'
HEALTHY_WITHIN = 120

parser = argparse.ArgumentParser()
parser.add_argument('image')
parser.add_argument('--engine', choices=['docker', 'podman'], default='podman')
parser.add_argument('--network', choices=['host', 'publish'], default='publish')
args = parser.parse_args()
suffix = secrets.token_hex(4)
NAME, VOLUME = f'workboard-smoke-{suffix}', f'workboard-smoke-{suffix}'


def engine(*command, check=True):
    result = subprocess.run([args.engine, *command], capture_output=True, text=True)
    if check and result.returncode != 0:
        raise RuntimeError(f'{args.engine} {" ".join(command)}: {result.stderr.strip()}')
    return result


def call(path, token=None, host=LOCAL, extra=None):
    headers = dict(extra or {}, Host=host)
    if token is not None: headers['Authorization'] = 'Bearer ' + token
    try:
        response = urlopen(Request(f'http://{LOCAL}{path}', headers=headers), timeout=10)
        return response.status, response.read()
    except HTTPError as error:
        return error.code, error.read()


def wait_healthy():
    deadline = time.monotonic() + HEALTHY_WITHIN
    while time.monotonic() < deadline:
        # Podman runs health checks from systemd timers, which a session
        # without systemd lacks; run one directly instead.
        if args.engine == 'podman':
            if engine('healthcheck', 'run', NAME, check=False).returncode == 0: return
        elif engine('inspect', '--format', '{{.State.Health.Status}}', NAME).stdout.strip() == 'healthy':
            return
        if engine('inspect', '--format', '{{.State.Running}}', NAME).stdout.strip() != 'true':
            raise RuntimeError('The container stopped:\n' + engine('logs', NAME).stderr)
        time.sleep(1)
    raise RuntimeError(f'Not healthy within {HEALTHY_WITHIN}s:\n' + engine('inspect', NAME).stdout)


def wait_port_free():
    """Wait for the stopped container's port forwarder to let go of the
    port, as a restart's new one would otherwise fail to bind it."""
    for _ in range(100):
        with socket.socket() as sock:
            # As the forwarder binds it: past the closed connections' TIME_WAIT.
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            try:
                sock.bind(('127.0.0.1', PORT))
                return
            except OSError:
                time.sleep(.1)
    raise RuntimeError(f'Port {PORT} is still taken')


def service_token():
    """The service's token, rotated on every start, from the data
    directory: the image has no shell to read it with."""
    archive = subprocess.run([args.engine, 'cp', f'{NAME}:{DATA}/service.token', '-'], capture_output=True, check=True).stdout
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        return tar.extractfile('service.token').read().decode().strip()


def companion_token():
    return engine('exec', NAME, 'workboard', 'companion-token').stdout.strip()


def check_api():
    token = service_token()
    status, body = call('/')
    assert status == 200 and b'<html' in body.lower(), (status, body[:200])
    assert call('/api/snapshot')[0] == 401
    status, body = call('/api/snapshot', token)
    assert status == 200 and json.loads(body)['mode'] == 'hub', (status, body)
    # Through `tailscale serve`, which passes on the tailnet name.
    assert call('/api/snapshot', host=TAILNET)[0] == 401
    assert call('/api/snapshot', token, host=TAILNET)[0] == 200
    assert call('/api/snapshot', token, host='attacker.example')[0] == 403
    assert call('/api/snapshot', token, host=TAILNET, extra={'Tailscale-Funnel-Request': '?1'})[0] == 403
    companion = companion_token()
    assert call('/api/export/external', companion)[0] == 200
    assert call('/api/snapshot', companion)[0] == 403
    return companion


network = ['--network', 'host'] if args.network == 'host' else [
    '--network', 'pasta:--host-lo-to-ns-lo', '--publish', f'127.0.0.1:{PORT}:{PORT}']
try:
    engine('volume', 'create', VOLUME)
    engine('run', '--detach', '--name', NAME, '--volume', f'{VOLUME}:{DATA}', *network,
           '--cap-drop', 'all', '--security-opt', 'no-new-privileges', '--read-only', '--tmpfs', '/tmp',
           '--health-interval', '2s', args.image, 'serve', '--hub', '--allowed-host', TAILNET)
    user = engine('inspect', '--format', '{{.Config.User}}', NAME).stdout.strip()
    assert user.split(':')[0] not in ('', '0', 'root'), f'Runs as {user!r}'
    wait_healthy()
    companion = check_api()
    engine('stop', NAME)
    wait_port_free()
    engine('start', NAME)
    wait_healthy()
    assert check_api() == companion, 'The companion token did not survive a restart'
    print(f'{args.image}: healthy as user {user}, API checks passed, state kept across a restart')
except Exception:
    logs = engine('logs', NAME, check=False)
    print(logs.stdout, logs.stderr)
    raise
finally:
    engine('rm', '--force', NAME, check=False)
    engine('volume', 'rm', VOLUME, check=False)
