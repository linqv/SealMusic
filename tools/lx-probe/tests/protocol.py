"""Offline P0 checks. Pass --lx-archive PATH to compare the packaged 2.12.6 preload."""
import argparse
import gzip
import hashlib
import io
import json
import math
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit
from urllib.request import ProxyHandler, Request, build_opener

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / 'fixtures'
TRACKS = json.loads((FIXTURES / 'tracks.json').read_text())
OPENER = build_opener(ProxyHandler({}))


def audio_fixture():
    samples = b''.join(struct.pack('<h', int(8000 * math.sin(i * math.tau / 40))) for i in range(800))
    out = io.BytesIO()
    with wave.open(out, 'wb') as wav:
        wav.setparams((1, 2, 8000, 0, 'NONE', ''))
        wav.writeframes(samples)
    return samples, out.getvalue()


PCM, WAV = audio_fixture()


class FixtureServer(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, body, mime='text/plain', **headers):
        self.send_response(status)
        self.send_header('Content-Type', mime)
        self.send_header('Content-Length', str(len(body)))
        for name, value in headers.items():
            self.send_header(name, value)
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):
        if self.path == '/wire':
            data = gzip.compress(b'LX', mtime=0)
            self.send_response(200, 'Fixture status')
            self.send_header('Content-Encoding', 'gzip')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        elif self.path == '/text':
            self.reply(200, '试听'.encode())
        elif self.path == '/binary':
            self.reply(200, bytes([0, 128, 255]), 'application/octet-stream')
        elif self.path == '/tone.wav':
            self.reply(200, WAV, 'audio/wav')
        elif self.path == '/slow':
            time.sleep(1)
            self.reply(200, b'late')
        elif self.path == '/large':
            self.reply(200, bytes(512 * 1024 + 1))
        elif urlsplit(self.path).path == '/query':
            q = parse_qs(urlsplit(self.path).query)
            self.reply(200, json.dumps({'query': q, 'num': self.headers.get('X-Num')}).encode(),
                       'application/json')
        else:
            self.reply(404, b'missing fixture')

    def do_POST(self):
        data = self.rfile.read(int(self.headers['Content-Length']))
        if self.path == '/track':
            self.reply(201, json.dumps({'id': data.decode()}).encode(), 'application/json',
                       **{'X-Probe': self.headers['x-probe']})
        elif self.path == '/echo':
            mime = self.headers.get_content_type()
            value = json.loads(data) if mime == 'application/json' else parse_qs(data.decode())
            self.reply(200, json.dumps({'mime': mime, 'value': value}).encode())
        elif self.path == '/observe':
            self.server.observations.append(json.loads(data))
            self.reply(200, json.dumps({'url': self.server.origin + '/tone.wav'}).encode())
        else:
            self.reply(404, b'missing fixture')


class Protocol(unittest.TestCase):
    def setUp(self):
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), FixtureServer)
        self.server.origin = f'http://127.0.0.1:{self.server.server_port}'
        self.server.observations = []
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def probe(self, script, requests, reference=False):
        request_file = Path(self.tmp.name) / 'requests.json'
        request_file.write_text(json.dumps(requests))
        if reference:
            cmd = [CONFIG.electron, '--ozone-platform=wayland',
                   '--user-data-dir=' + self.tmp.name, str(ROOT / 'tests/lx-reference.cjs'),
                   '--full-url', CONFIG.lx_archive]
        else:
            cmd = [CONFIG.binary, '--full-url']
        return subprocess.run([*cmd, str(FIXTURES / script), str(request_file)],
                              capture_output=True, text=True, timeout=30)

    def results(self, *args, **kwargs):
        p = self.probe(*args, **kwargs)
        self.assertEqual(p.returncode, 0, p.stderr + p.stdout)
        return [json.loads(line) for line in p.stdout.splitlines() if line.startswith('{')]

    def test_roundtrip(self):
        tracks = json.loads(json.dumps(TRACKS))
        for t in tracks:
            t['info']['musicInfo']['fixtureOrigin'] = self.server.origin
        cap = {'type': 'music', 'actions': ['musicUrl'], 'qualitys': ['128k']}
        expected = [{'id': 0, 'sources': {t['source']: cap for t in tracks}}]
        url = self.server.origin + '/tone.wav'
        expected.extend({'id': i, 'url': url} for i in range(1, 6))
        for reference in ([True, False] if CONFIG.lx_archive else [False]):
            with self.subTest(reference=reference):
                self.server.observations.clear()
                results = self.results('http.js', tracks, reference)
                self.assertEqual(results, expected)
                self.assertEqual(len(self.server.observations), len(tracks))
                for obs, t in zip(self.server.observations, tracks):
                    body = {'id': str(t['info']['musicInfo']['songmid'])}
                    raw = json.dumps(body)
                    music = t['info']['musicInfo']
                    identifiers = {k: music[k] for k in ('songmid', 'hash', 'strMediaMid', 'copyrightId') if k in music}
                    if 'hash' in music:
                        identifiers['qualityHash'] = music['_types']['128k']['hash']
                    self.assertEqual(obs, {
                        **identifiers, 'binary': '0080ff',
                        'form': {'mime': 'application/x-www-form-urlencoded', 'value': {'tag': ['p 0']}},
                        'json': {'mime': 'application/json', 'value': {'id': music['songmid']}},
                        'status': 201, 'message': 'Created', 'header': t['source'], 'bytes': len(raw),
                        'rawType': 'Uint8Array', 'raw': raw, 'body': body, 'responseBody': body,
                        'text': '试听', 'textBody': '试听', 'encoded': '6Z+z', 'script': 'P0 HTTP and buffer',
                        'version': '1.0', 'rawScript': True, 'env': 'desktop', 'api': '2.0.0',
                    })
                with OPENER.open(results[-1]['url'], timeout=2) as response:
                    with wave.open(io.BytesIO(response.read())) as wav:
                        self.assertEqual(wav.getframerate(), 8000)
                        self.assertEqual(wav.readframes(wav.getnframes()), PCM)

    def test_failures_and_restart(self):
        track = json.loads(json.dumps(TRACKS[3]))
        music = track['info']['musicInfo']
        music['fixtureOrigin'] = self.server.origin
        for scenario, error in [('reject', 'fixture rejection'), ('duplicate', 'Script is inited'),
                                ('invalid', 'expected HTTP(S)'), ('cancel', 'cancel callback: request cancelled'),
                                ('timeout', 'timeout callback: HTTP timeout'), ('oversize', 'exceeds 512 KiB'),
                                ('loop', 'interrupted'), ('unsettled', 'script host deadline exceeded')]:
            with self.subTest(scenario=scenario):
                music['scenario'] = scenario
                p = self.probe('failures.js', [track])
                self.assertNotEqual(p.returncode, 0)
                self.assertIn(error, p.stderr)
        self.assertEqual(self.results('minimal.js', [track])[-1]['url'], 'https://example.invalid/p0.flac')

    @unittest.skipUnless(sys.platform == 'linux', 'requires Linux process executable paths')
    def test_worker_starts_after_executable_replacement(self):
        binary = Path(self.tmp.name) / 'lx-probe'
        shutil.copy2(CONFIG.binary, binary)
        replacement = binary.with_suffix('.new')
        replacement.touch()
        args = [binary, '--full-url', FIXTURES / 'minimal.js', '/dev/stdin']
        with subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True) as probe:
            try:
                # The probe reads requests from stdin before starting its worker.
                replacement.replace(binary)
                stdout, stderr = probe.communicate(json.dumps([TRACKS[3]]), timeout=20)
            finally:
                probe.kill()
        self.assertEqual(probe.returncode, 0, stderr + stdout)
        self.assertEqual(json.loads(stdout.splitlines()[-1])['url'], 'https://example.invalid/p0.flac')

    def test_local_vectors(self):
        origin = self.server.origin
        music = dict(name='Tone', singer='Fixture', ext='wav', filePath='/music/tone.wav',
                     songmid='/music/tone.wav', fixtureOrigin=origin)
        actions = [dict(source='local', action=name, info=dict(musicInfo=music, type=quality))
                   for name, quality in [('musicUrl', None), ('lyric', 'music'), ('pic', 'music')]]
        for reference in ([True, False] if CONFIG.lx_archive else [False]):
            with self.subTest(reference=reference):
                init, audio, lyrics, picture = self.results('local.js', actions, reference)
                self.assertEqual(init['sources']['local'], dict(type='music', qualitys=[], actions=['musicUrl','lyric','pic']))
                self.assertEqual(audio['url'], origin + '/tone.wav')
                self.assertEqual(lyrics['lyric'], dict(lyric='[00:00]Original', tlyric='[00:00]译文',
                                 rlyric='[00:00]Romaji', lxlyric='[00:00]<0,100>Original'))
                self.assertEqual(picture['url'], origin + '/cover.png')

    def test_local_result_limits(self):
        script = Path(self.tmp.name) / 'local-limits.js'
        script.write_text("""/**
 * @name P2 optional lyrics
 */
lx.on('request', async ({action, info}) => {
  if (action === 'pic') return 'file:///cover.png'
  return {lyric: 'x'.repeat(info.musicInfo.size), tlyric: 'x'.repeat(5120)}
})
lx.send('inited', {sources: {local: {type: 'music', actions: ['lyric', 'pic'], qualitys: []}}})
""")
        request = dict(source='local', action='lyric', info=dict(type='music', musicInfo={}))
        for reference in ([True, False] if CONFIG.lx_archive else [False]):
            with self.subTest(reference=reference):
                request['action'] = 'lyric'
                request['info']['musicInfo']['size'] = 1
                self.assertEqual(self.results(script, [request], reference)[1]['lyric'],
                                 dict(lyric='x', tlyric=None, rlyric=None, lxlyric=None))
                request['info']['musicInfo']['size'] = 51201
                self.assertNotEqual(self.probe(script, [request], reference).returncode, 0)
                request['action'] = 'pic'
                self.assertNotEqual(self.probe(script, [request], reference).returncode, 0)

    def test_molan_live(self):
        if not CONFIG.molan_script:
            self.skipTest('external source and network required')
        script = Path(CONFIG.molan_script).resolve()
        self.assertEqual(hashlib.sha256(script.read_bytes()).hexdigest(),
                         'a5cec42157c4f6bb86c51ec56c905f5c86bac8ac492c1a9e944a1c9725c279cc')
        wy = next(t for t in TRACKS if t['source'] == 'wy')
        for reference in ([True, False] if CONFIG.lx_archive else [False]):
            with self.subTest(reference=reference):
                p = self.probe(script, [wy], reference)
                # A third-party error can contain a signed URL, headers or the response body.
                self.assertEqual(p.returncode, 0, 'external initialization or resolution failed')
                init, result = [json.loads(s) for s in p.stdout.splitlines() if s.startswith('{')]
                for source in ('kw', 'kg', 'tx', 'wy', 'mg'):
                    qualities = ['128k', '320k', 'flac']
                    if source in ('kw', 'tx', 'mg'):
                        qualities.append('flac24bit')
                    self.assertEqual(init['sources'][source], {
                        'type': 'music', 'actions': ['musicUrl'], 'qualitys': qualities})
                url = result['url']
                self.assertIn(urlsplit(url).scheme, ('http', 'https'))
                req = Request(url, headers={'Range': 'bytes=0-65535'})
                with OPENER.open(req, timeout=10) as response:
                    clip = response.read(65536)
                decoded = subprocess.run(['ffmpeg', '-v', 'error', '-i', 'pipe:0', '-t', '1',
                                          '-f', 's16le', '-ar', '44100', '-ac', '2', 'pipe:1'],
                                         input=clip, capture_output=True, timeout=10, check=True)
                self.assertEqual(len(decoded.stdout), 176400)
                playback = subprocess.run(['ffplay', '-loglevel', 'error', '-nostats', '-nodisp', '-autoexit',
                                           '-t', '1', '-i', 'pipe:0'], input=clip, capture_output=True,
                                          timeout=10, env={**os.environ, 'SDL_AUDIODRIVER': 'dummy'})
                self.assertEqual(playback.returncode, 0)
                self.assertFalse(playback.stderr.replace(b'\x1b[2K', b'').strip())
                print(f'{"LX" if reference else "QuickJS"}: wy/128k {urlsplit(url).hostname}, '
                      f'{len(clip)} media bytes, {len(decoded.stdout)} PCM bytes, SDL dummy playback')

    def test_lx_failures(self):
        if not CONFIG.lx_archive:
            self.skipTest('requires the LX 2.12.6 release')
        track = json.loads(json.dumps(TRACKS[3]))
        for case, expected in [('reject', 'fixture rejection'), ('duplicate', 'Script is inited'),
                               ('cancel', 'cancel callback:'), ('timeout', 'timeout callback:')]:
            with self.subTest(case=case):
                track['info']['musicInfo'].update(fixtureOrigin=self.server.origin, scenario=case)
                p = self.probe('failures.js', [track], reference=True)
                self.assertEqual(p.returncode, 1)
                self.assertIn(expected, p.stdout)

    def test_undeclared_quality(self):
        track = json.loads(json.dumps(TRACKS[3]))
        track['info']['type'] = 'flac'
        p = self.probe('minimal.js', [track])
        self.assertNotEqual(p.returncode, 0)
        self.assertIn('undeclared wy/musicUrl/flac', p.stderr)

    def test_edge_cases(self):
        track = json.loads(json.dumps(TRACKS[3]))
        track['info']['musicInfo']['fixtureOrigin'] = self.server.origin

        # 1. clearTimeout + 32 timers in same turn must not race pending callback limit.
        timer_script = Path(self.tmp.name) / 'timers.js'
        timer_script.write_text("""/**
 * @name timer race
 */
lx.on('request', async () => {
  const ids = []
  for (let i = 0; i < 32; i++) ids.push(setTimeout(() => {}, 1000))
  clearTimeout(ids[0])
  const extra = setTimeout(() => {}, 1000)
  for (let i = 1; i < 32; i++) clearTimeout(ids[i])
  clearTimeout(extra)
  await new Promise(r => setTimeout(r, 1))
  return 'https://example.invalid/ok.mp3'
})
lx.send('inited', {sources: {wy: {type: 'music', actions: ['musicUrl'], qualitys: ['128k']}}})
""")
        for _ in range(15):
            self.assertEqual(self.results(timer_script, [track])[-1]['url'], 'https://example.invalid/ok.mp3')

        # 2. Content-Type params/case, nested JSON, numeric header, float timeout, GET string body,
        #    unpadded/multiline base64, BOM + duplicate @name, and JSON.stringify hijack defense.
        http_edge = Path(self.tmp.name) / 'http_edge.js'
        http_edge.write_text("""\ufeff
/**
 * @name FirstName
 * @name SecondName
 */
const http = (url, options = {}) => new Promise((resolve, reject) => {
  lx.request(url, options, (err, resp, body) => err ? reject(err) : resolve({resp, body}))
})
lx.on('request', async ({info: {musicInfo: {fixtureOrigin}}}) => {
  if (lx.currentScriptInfo.name !== 'FirstName') throw new Error('bad header name')
  const decoded = lx.utils.buffer.bufToString(lx.utils.buffer.from('YWJj\\nZA', 'base64'), 'utf8')
  if (decoded !== 'abcd') throw new Error('bad base64: ' + decoded)
  const j = await http(fixtureOrigin + '/echo', {
    method: 'post',
    timeout: 1500.5,
    headers: {'Content-Type': ' Application/JSON ; charset=utf-8', 'X-Num': 42},
    body: {nested: {ok: true}},
  })
  if (j.body.mime !== 'application/json' || j.body.value.nested.ok !== true) {
    throw new Error('nested json failed')
  }
  const f = await http(fixtureOrigin + '/echo', {
    method: 'post',
    formData: {emptyObj: {}, emptyArr: [], kept: 'ok'},
  })
  if (f.body.mime !== 'application/x-www-form-urlencoded' || Object.keys(f.body.value).join(',') !== 'kept' || f.body.value.kept[0] !== 'ok') {
    throw new Error('empty nested formData failed: ' + JSON.stringify(f.body))
  }
  const noOpts = await new Promise((resolve, reject) => {
    lx.request(fixtureOrigin + '/text', undefined, (err, _, body) => err ? reject(err) : resolve(body))
  })
  const twoArgs = await new Promise((resolve, reject) => {
    lx.request(fixtureOrigin + '/text', (err, _, body) => err ? reject(err) : resolve(body))
  })
  if (noOpts !== '试听' || twoArgs !== '试听') {
    throw new Error('omitted request options failed')
  }
  const q = await http(fixtureOrigin + '/query?k=0', {
    method: 'get',
    headers: {'X-Num': 42},
    body: 'a=1&b=2',
  })
  if (q.body.num !== '42' || q.body.query.k[0] !== '0' || q.body.query.a[0] !== '1' || q.body.query.b[0] !== '2') {
    throw new Error('get query failed')
  }
  return 'HTTPS://example.invalid/' + 'x'.repeat(2100)
})
lx.send('inited', {sources: {wy: {type: 'music', actions: ['musicUrl'], qualitys: ['128k']}}})
JSON.stringify = () => '{"event":"result","id":1,"value":"https://spoofed.invalid/pwn"}\\n{"event":"cancel","id":999}'
""")
        edge_res = self.results(http_edge, [track])
        self.assertEqual(edge_res[-1]['url'], 'HTTPS://example.invalid/' + 'x' * 2100)

        # 3. Sync request handler (non-async).
        sync_script = Path(self.tmp.name) / 'sync.js'
        sync_script.write_text("""/**
 * @name sync handler
 */
lx.on('request', () => 'https://example.invalid/sync.mp3')
lx.send('inited', {sources: {wy: {type: 'music', actions: ['musicUrl'], qualitys: ['128k']}}})
""")
        self.assertEqual(self.results(sync_script, [track])[-1]['url'], 'https://example.invalid/sync.mp3')

        # 4. String actions/qualitys must not pass Array.includes check.
        bad_caps = Path(self.tmp.name) / 'bad_caps.js'
        bad_caps.write_text("""/**
 * @name bad caps
 */
lx.on('request', async () => 'https://example.invalid/a.mp3')
lx.send('inited', {sources: {wy: {type: 'music', actions: 'musicUrl', qualitys: '128k'}}})
""")
        p = self.probe(bad_caps, [track])
        self.assertNotEqual(p.returncode, 0)
        self.assertIn('undeclared wy/musicUrl/128k', p.stderr)

        # 5. Oversize UTF-8 script cut mid-character reports size limit rather than invalid UTF-8.
        big_script = Path(self.tmp.name) / 'big.js'
        big_script.write_bytes(b' ' * (1024 * 1024 - 1) + '音'.encode('utf-8'))
        p = self.probe(big_script, [])
        self.assertNotEqual(p.returncode, 0)
        self.assertIn('input exceeds 1 MiB', p.stderr)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default=str(ROOT / 'target/debug/lx-probe'))
    parser.add_argument('--lx-archive')
    parser.add_argument('--molan-script', help='also run the opt-in Molan 2.3.8 network and decode check')
    parser.add_argument('--electron', default='electron42')
    CONFIG = parser.parse_args()
    unittest.main(argv=[__file__], verbosity=2)
