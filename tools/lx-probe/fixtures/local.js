/**
 * @name P2 local source
 * @version 1.0
 */
const {utils: {buffer, crypto, zlib}} = window.lx
const hex = data => buffer.bufToString(data, 'hex')
const equal = (value, expected, label) => {
  if (value !== expected) throw new Error(label + ': ' + value)
}
const fails = async (fn, label) => {
  try { await fn() } catch { return }
  throw new Error('expected error: ' + label)
}
const publicKey = `-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQC9XXfDgUmi2IzQbfX7kmnnfcZL
1dwBdomuUkAEta8xmYet0w7hyg1yrCHDSPMUKRWg0rIHTDhp1M4dZj/BkZ/xK2Ah
j+EWG+7gJAjAIUsw0nU12FbvaL7KelSYzz+rQu+xVMRJ8YUqF85dCzO10oekt7hW
3QfpR0mHVO8TYBJM3wIDAQAB
-----END PUBLIC KEY-----`
async function vectors(origin) {
  const input = buffer.from('音乐')
  equal(hex(input), 'e99fb3e4b990', 'UTF-8')
  equal(hex(buffer.from('é音', 'latin1')), 'e9f3', 'latin1')
  equal(hex(buffer.from('音乐', 'utf16le')), 'f397504e', 'UTF-16LE')
  equal(hex(buffer.from('-_8', 'base64')), 'fbff', 'base64url')
  equal(hex(buffer.from('aaXY', 'hex')), 'aa', 'partial hex')
  equal(crypto.md5('音乐'), '95521bb744463633c858cc50abb44e7d', 'MD5')
  const key = buffer.from('0123456789abcdef')
  equal(hex(crypto.aesEncrypt(input, 'aes-128-cbc', key, buffer.from('abcdef0123456789'))),
    '8c8fad9c5a1b0a87d0aa36de245255f9', 'AES-CBC')
  equal(hex(crypto.aesEncrypt(input, 'aes-128-ecb', key, buffer.from([]))),
    '99e3af56ccd5507cabebf1e10e00ebae', 'AES-ECB')
  equal(hex(crypto.rsaEncrypt(buffer.from('fixture'), publicKey)),
    '9a62394ad5de511b2cc88d2a7595331fe7d0c71798bfdaab01b16e137619427e0484eb7ea8c44a0a352769fb3845e7a3d3d1a6eba63d5c27cf6e98df77f01d9a4a77a0d08d9d2992ab9f2885cb97b9d27d1ddf9295e1b4347930d3c8a60e86261a14e530bda0ef3437c626656eb7707aec9e8f2ec53826ee2bf857fd6351e9c4', 'RSA')
  const random = crypto.randomBytes(17)
  equal(random.constructor.name, 'Uint8Array', 'random type')
  equal(random.length, 17, 'random length')
  equal(crypto.randomBytes(0).length, 0, 'empty random bytes')
  const compressed = await zlib.deflate(input)
  equal(hex(compressed), '789c7b397ff3939d130010110469', 'zlib')
  equal(hex(await zlib.inflate(compressed)), hex(input), 'inflate')
  await fails(() => zlib.inflate(buffer.from([1, 2])), 'invalid stream')
  await fails(() => crypto.aesEncrypt(input, 'aes-128-cbc', key, input), 'invalid IV')
  await fails(() => crypto.rsaEncrypt(input, 'invalid'), 'invalid public key')
  await fails(() => crypto.randomBytes(-1), 'invalid length')
  const wire = await new Promise((ok, fail) => lx.request(origin + '/wire', {},
    (err, response) => err ? fail(err) : ok(response)))
  equal(wire.statusMessage, 'Fixture status', 'HTTP reason phrase')
  equal(hex(wire.raw).slice(0, 6), '1f8b08', 'compressed wire bytes')
  equal(wire.bytes, wire.raw.length, 'wire byte count')
  const echo = await new Promise((ok, fail) => lx.request(origin + '/echo', {
    method: 'post', formData: {nested: {tag: ['音', 'b']}, empty: null}
  }, (err, _, body) => err ? fail(err) : ok(body)))
  equal(echo.mime, 'application/x-www-form-urlencoded', 'formData MIME')
  equal(echo.value['nested[tag][]'].join(','), '音,b', 'nested formData')
}

lx.on('request', async ({source, action, info: {type, musicInfo}}) => {
  equal(source, 'local', 'source')
  equal(musicInfo.filePath, musicInfo.songmid, 'local ID')
  equal(musicInfo.ext, 'wav', 'extension')
  const origin = musicInfo.fixtureOrigin
  if (action === 'musicUrl') {
    equal(type, null, 'quality')
    await vectors(origin)
    await lx.send('updateAlert', {log: 'Fixture update', updateUrl: origin + '/update'})
    await fails(() => lx.send('updateAlert', {log: 'duplicate'}), 'duplicate notice')
    return origin + '/tone.wav'
  }
  equal(type, 'music', 'detail type')
  if (action === 'pic') return origin + '/cover.png'
  if (action === 'lyric') return {lyric: '[00:00]Original', tlyric: '[00:00]译文',
    rlyric: '[00:00]Romaji', lxlyric: '[00:00]<0,100>Original'}
})
lx.send('inited', {sources: {local: {type: 'music', actions: ['musicUrl','lyric','pic'], qualitys: []}}})
