"""Run inside dbus-run-session and Xvfb. Uses a private library of silent WAVs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import tempfile
import threading
import time
import wave
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PLAYER = 'org.mpris.MediaPlayer2.Player'
SERVICE = 'org.mpris.MediaPlayer2.sealmusic'
OBJECT = '/org/mpris/MediaPlayer2'
ROOT = Path(__file__).resolve().parents[3]


def wait_for(check, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(.08)
    raise AssertionError(f'timed out waiting for {check}')


def cli(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.DEVNULL).strip()


def prop(name):
    try:
        reply = cli('busctl', '--user', '--json=short', 'get-property', SERVICE, OBJECT, PLAYER, name)
        return json.loads(reply)['data']
    except subprocess.CalledProcessError:
        return None


def player(method):
    cli('busctl', '--user', 'call', SERVICE, OBJECT, PLAYER, method)


def png():
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 1, 1, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(b'\0\x19\xa1\x86')) + chunk(b'IEND', b''))


class Server(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.server.paths.append(self.path)
        if self.path == '/hold':
            self.server.held.set()
            self.server.release.wait(8)
        body = {'/art': png(), '/oversize': bytes(4 * 1024 * 1024 + 1),
                '/media': self.server.media}.get(self.path, b'{}')
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass


def run(work, binary, captures):
    library = work / 'music'
    library.mkdir()
    for title in ['A', 'B', 'C']:
        with wave.open(str(library / f'{title}.wav'), 'wb') as audio:
            audio.setparams((1, 2, 8000, 0, 'NONE', ''))
            audio.writeframes(bytes(8000 * 2 * 60))
    before = {p: hashlib.sha256(p.read_bytes()).digest() for p in library.iterdir()}
    server = ThreadingHTTPServer(('127.0.0.1', 0), Server)
    server.media = (library / 'A.wav').read_bytes()
    server.held, server.release, server.paths = threading.Event(), threading.Event(), []
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f'http://127.0.0.1:{server.server_port}'
    script = '''/**
 * @name Local desktop fixture
 * @version 1.0
 */
const origin = ORIGIN
lx.on('request', async ({action, info: {type, musicInfo: {name}}}) => {
  if (action === 'musicUrl') {
    if (type !== null) throw Error('invalid quality')
    await lx.send('updateAlert', {log: 'Fixture update', updateUrl: origin + '/update'})
    return origin + '/media'
  }
  if (action === 'pic') return origin + (name === 'A' ? '/art' : '/oversize')
  if (name === 'A') return {lyric: '[00:00]First line', tlyric: '[00:00]第一行'}
  if (name === 'C') await new Promise(ok => lx.request(origin + '/hold', {}, ok))
  throw Error('no lyrics')
})
lx.send('inited', {sources: {local: {type: 'music', actions: ['musicUrl','lyric','pic'], qualitys: []}}})
'''.replace('ORIGIN', json.dumps(origin))
    script_id = hashlib.sha256(script.encode()).hexdigest()
    sources = work / 'data/sealmusic/sources'
    sources.mkdir(parents=True)
    (sources / f'{script_id}.js').write_text(script)
    cfg = work / 'config/sealmusic'
    cfg.mkdir(parents=True)
    (cfg / 'config.toml').write_text(f'''song_dir = "{library}"
current_page = 3
lang = "zh_CN"
volume = 0.0
follow_system_theme = false
light_ui = true
[lx_script]
id = "{script_id}"
enabled = false
platforms = {{}}
metadata = {{name = "Local desktop fixture"}}
''')
    env = dict(os.environ, XDG_CONFIG_HOME=str(work / 'config'), XDG_DATA_HOME=str(work / 'data'),
               XDG_CACHE_HOME=str(work / 'cache'), HOME=str(work), WINIT_UNIX_BACKEND='x11')
    for key in ('WAYLAND_DISPLAY', 'http_proxy', 'https_proxy', 'all_proxy',
                'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY'):
        env.pop(key, None)
    with (work / 'app.log').open('w+') as log:
        app = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            def metadata():
                assert app.poll() is None, (work / 'app.log').read_text()
                return prop('Metadata') or {}
            def includes(key, value):
                return value in str(metadata().get(key, ''))
            def capture(label):
                if captures:
                    time.sleep(.25)
                    cli('import', '-window', window, str(captures / f'{label}.png'))
            wait_for(lambda: includes('xesam:title', 'A'))
            window = wait_for(lambda: subprocess.run(['xdotool', 'search', '--onlyvisible', '--name', '^SealMusic$'],
                              text=True, capture_output=True).stdout.strip()).splitlines()[0]
            cli('xdotool', 'windowsize', window, '900', '510')
            cli('xdotool', 'windowfocus', window)
            assert 'mpris:artUrl' not in metadata() and not server.paths
            # Re-enable a source saved with the P1 configuration shape. The app
            # must import its local-only capability declaration from the JS file.
            time.sleep(.2)
            cli('xdotool', 'mousemove', '--window', window, '242', '290', 'click', '1')
            wait_for(lambda: includes('mpris:artUrl', 'file:'))
            capture('settings')
            cli('xdotool', 'mousemove', '--window', window, '450', '300',
                'click', '5', 'click', '5', 'click', '5', 'click', '5',
                'click', '5', 'click', '5', 'click', '5', 'click', '5')
            time.sleep(.15)
            cli('xdotool', 'mousemove', '--window', window, '285', '405', 'click', '1')
            cli('xdotool', 'key', 'F2')
            time.sleep(.25)
            cli('xdotool', 'mousemove', '--window', window, '240', '84', 'click', '3')
            capture('source-menu-en')
            cli('xdotool', 'key', 'Escape', 'F4')
            time.sleep(.25)
            cli('xdotool', 'mousemove', '--window', window, '450', '300',
                'click', '5', 'click', '5', 'click', '5', 'click', '5',
                'click', '5', 'click', '5', 'click', '5', 'click', '5')
            time.sleep(.15)
            cli('xdotool', 'mousemove', '--window', window, '675', '405', 'click', '1')
            cli('xdotool', 'key', 'F3')
            lyrics_key = next(k for k in metadata() if k.endswith(':syncedLyrics'))
            assert includes(lyrics_key, 'First line')
            old_art = next((work / 'cache/sealmusic/art').iterdir())
            player('Play')
            wait_for(lambda: prop('PlaybackStatus') == 'Playing' and prop('Position') > 100000)
            capture('A-lyrics')
            player('Next')
            wait_for(lambda: includes('xesam:title', 'B'))
            wait_for(lambda: '/oversize' in server.paths)
            assert lyrics_key not in metadata() and 'mpris:artUrl' not in metadata() and not old_art.exists()
            wait_for(lambda: prop('Position') > 100000 and prop('PlaybackStatus') == 'Playing')
            capture('B-cleared')
            player('Next')
            assert server.held.wait(5), 'C did not start its details request'
            # Terminate only this application's transient script worker.
            workers = {int(pid) for thread in Path(f'/proc/{app.pid}/task').iterdir()
                       for pid in (thread / 'children').read_text().split()}
            worker = next(pid for pid in workers if b'--lx-worker' in Path(f'/proc/{pid}/cmdline').read_bytes())
            os.kill(worker, signal.SIGKILL)
            server.release.set()
            wait_for(lambda: includes('xesam:title', 'C') and prop('Position') > 200000)
            assert prop('PlaybackStatus') == 'Playing' and lyrics_key not in metadata()
            assert '/media' not in server.paths
            assert before == {p: hashlib.sha256(p.read_bytes()).digest() for p in library.iterdir()}
            print('A → B → C: local audio advanced, stale artwork/lyrics cleared, oversized cover and worker exit isolated')
            cli('xdotool', 'windowfocus', window, 'key', 'F2')
            time.sleep(.3)
            cli('xdotool', 'mousemove', '--window', window, '240', '84', 'click', '3')
            capture('source-menu')
            # Activate the only item of the row's context menu.
            cli('xdotool', 'key', 'Down', 'Return')
            wait_for(lambda: '/media' in server.paths)
            wait_for(lambda: includes('xesam:title', 'A') and prop('PlaybackStatus') == 'Playing'
                     and prop('Position') > 100000)
            assert '/update' not in server.paths
            capture('source-update')
            assert before == {p: hashlib.sha256(p.read_bytes()).digest() for p in library.iterdir()}
            print('Explicit local.musicUrl decoded; update link waited for a click; three file hashes unchanged')
        finally:
            server.release.set()
            app.terminate()
            app.wait(timeout=5)
            server.shutdown()
            server.server_close()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/sealmusic')
    parser.add_argument('--captures', type=Path)
    args = parser.parse_args()
    if args.captures:
        args.captures.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        run(Path(tmp), args.binary.resolve(), args.captures)
