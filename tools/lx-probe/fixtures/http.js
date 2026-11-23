/**
 * @name P0 HTTP and buffer
 * @version 1.0
 */
const {EVENT_NAMES, currentScriptInfo, utils, on, send, request} = globalThis.lx
const http = (url, options = {}) => new Promise((resolve, reject) => {
  request(url, options, (err, resp, body) => err ? reject(err) : resolve({resp, body}))
})
const sources = Object.fromEntries(['kw', 'kg', 'tx', 'wy', 'mg'].map(source =>
  [source, {type: 'music', actions: ['musicUrl', 'unused'], qualitys: ['128k', 'unknown']}]))

on(EVENT_NAMES.request, async ({source, action, info: {type, musicInfo}}) => {
  if (musicInfo.source !== source || type !== musicInfo.types[0].type || action !== 'musicUrl') {
    throw new Error('incorrect request shape')
  }
  const origin = musicInfo.fixtureOrigin
  const {resp, body} = await http(origin + '/track', {
    method: 'post', headers: {'x-probe': source}, body: String(musicInfo.songmid),
  })
  const text = await http(origin + '/text')
  const binary = await http(origin + '/binary')
  const form = await http(origin + '/echo', {method: 'post', form: {tag: 'p 0'}})
  const json = await http(origin + '/echo', {
    method: 'post', headers: {'Content-Type': 'application/json'}, body: {id: musicInfo.songmid},
  })
  const observation = {
    form: form.body, json: json.body,
    songmid: musicInfo.songmid, hash: musicInfo.hash, qualityHash: musicInfo._types[type].hash,
    strMediaMid: musicInfo.strMediaMid, copyrightId: musicInfo.copyrightId,
    binary: utils.buffer.bufToString(binary.resp.raw, 'hex'),
    status: resp.statusCode, message: resp.statusMessage, header: resp.headers['x-probe'],
    bytes: resp.bytes, rawType: resp.raw.constructor.name,
    raw: utils.buffer.bufToString(resp.raw, 'utf8'),
    body, responseBody: resp.body, text: text.body, textBody: text.resp.body,
    encoded: utils.buffer.bufToString(utils.buffer.from('音', 'utf8'), 'base64'),
    script: currentScriptInfo.name, version: currentScriptInfo.version,
    rawScript: currentScriptInfo.rawScript.startsWith('/**') && currentScriptInfo.rawScript.includes('fixtureOrigin'),
    env: lx.env, api: lx.version,
  }
  const reply = await http(origin + '/observe', {method: 'post', body: JSON.stringify(observation)})
  if (!reply.body.url) throw new Error('invalid fixture response')
  return reply.body.url
})

setTimeout(() => send(EVENT_NAMES.inited, {sources}), 1)
