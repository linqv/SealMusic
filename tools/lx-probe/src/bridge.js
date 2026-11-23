// Evaluated once in the worker. Rust retains the returned dispatcher privately.
(emitFrame, bytesFrom, bytesToString) => {
  const stringify = JSON.stringify
  const parseJson = JSON.parse
  const isArray = Array.isArray
  const resolvePromise = Promise.resolve.bind(Promise)
  const emit = frame => emitFrame(stringify(frame))
  const callbacks = new Map()
  let nextId = 1
  let initialized = false
  let handler
  let currentScriptInfo

  function enqueue(event, data, callback) {
    if (callbacks.size >= 32) throw new Error('too many pending callbacks')
    const id = nextId++
    callbacks.set(id, callback)
    emit({event, id, ...data})
    return id
  }
  function cancel(id) {
    if (callbacks.has(id)) emit({event: 'cancel', id})
  }
  globalThis.setTimeout = (fn, ms = 0, ...args) =>
    enqueue('timer', {ms: Math.max(0, Math.trunc(Number(ms) || 0))}, () => fn(...args))
  globalThis.clearTimeout = id => {
    cancel(id)
    callbacks.delete(id)
  }
  globalThis.console = Object.freeze({log() {}, info() {}, warn() {}, error() {}, debug() {}})

  const EVENT_NAMES = Object.freeze({request: 'request', inited: 'inited', updateAlert: 'updateAlert'})
  const buffer = {
    from(value, encoding = 'utf8') {
      if (typeof value === 'string') return bytesFrom(value, encoding)
      return new Uint8Array(value)
    },
    bufToString(value, encoding = 'utf8') {
      return bytesToString(value instanceof Uint8Array ? value : new Uint8Array(value), encoding)
    },
  }
  const lx = Object.freeze({
    EVENT_NAMES, env: 'desktop', version: '2.0.0',
    get currentScriptInfo() { return currentScriptInfo },
    utils: {buffer},
    async on(event, fn) {
      if (event !== 'request') throw new Error('unsupported lx.on: ' + event)
      handler = fn
    },
    async send(event, info) {
      if (event !== 'inited') throw new Error('unsupported lx.send: ' + event)
      if (initialized) throw new Error('Script is inited')
      initialized = true
      try {
        const sources = {}
        for (const source of ['kw', 'kg', 'tx', 'wy', 'mg']) {
          const entry = info.sources[source]
          if (entry?.type !== 'music') continue
          sources[source] = {
            type: 'music',
            actions: isArray(entry.actions) ? ['musicUrl'].filter(a => entry.actions.includes(a)) : [],
            qualitys: isArray(entry.qualitys)
              ? ['128k', '320k', 'flac', 'flac24bit'].filter(q => entry.qualitys.includes(q))
              : [],
          }
        }
        const {rawScript, ...metadata} = currentScriptInfo
        emit({event: 'result', id: 0, value: {sources, metadata}})
      } catch (err) {
        emit({event: 'error', id: 0, error: String(err)})
      }
    },
    request(url, options, callback) {
      const id = enqueue('http', {url, options}, msg => {
        if (msg.error) return callback(new Error(msg.error), null, null)
        const {raw, ...resp} = msg.response
        resp.raw = buffer.from(raw, 'base64')
        resp.body = buffer.bufToString(resp.raw)
        try { resp.body = parseJson(resp.body) } catch {}
        callback(null, resp, resp.body)
      })
      return () => cancel(id)
    },
  })
  globalThis.lx = lx
  return message => {
    switch (message.event) {
      case 'init': {
        const header = message.script.replace(/^\uFEFF/, '').trimStart().match(/^\/\*[\s\S]*?\*\//)?.[0]
        if (!header) throw new Error('missing script header')
        const info = {rawScript: message.script}
        for (const [key, limit] of Object.entries({name:24, description:36, version:36, author:56, homepage:1024})) {
          const field = header.split(/\r?\n/).map(line => /^\s?\*\s?@(\w+)\s(.+)$/.exec(line))
            .find(match => match && match[1] === key)
          const value = field?.[2].trim() ?? ''
          info[key] = value.length > limit ? value.slice(0, limit) + '...' : value
        }
        info.name ||= 'user_api_' + new Date().toLocaleString()
        currentScriptInfo = Object.freeze(info)
        break
      }
      case 'action': {
        const fail = err => emit({event: 'error', id: message.id, error: String(err?.message ?? err)})
        try {
          if (!handler) throw new Error('Request event is not defined')
          resolvePromise(handler.call(lx, message.data)).then(value => {
            if (typeof value !== 'string' || value.length > 8192 || !/^https?:/i.test(value)) {
              throw new Error('expected HTTP(S) musicUrl')
            }
            emit({event: 'result', id: message.id, value})
          }).catch(fail)
        } catch (err) { fail(err) }
        break
      }
      case 'callback': {
        const cb = callbacks.get(message.id)
        callbacks.delete(message.id)
        if (cb) cb(message)
        break
      }
    }
  }
}
