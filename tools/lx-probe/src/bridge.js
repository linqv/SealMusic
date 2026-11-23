// Evaluated once in the worker. Rust retains the returned dispatcher privately.
(emitFrame, bytesFrom, bytesToString, native) => {
  const stringify = JSON.stringify
  const parseJson = JSON.parse
  const isArray = Array.isArray
  const resolvePromise = Promise.resolve.bind(Promise)
  const emit = frame => emitFrame(stringify(frame))
  const callbacks = new Map()
  let nextId = 1
  let initialized = false
  let idleId
  let initResult
  let alerted = false
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
    from(value, encoding = 'utf8', length) {
      if (value instanceof ArrayBuffer) return new Uint8Array(value, Number(encoding) || 0, length)
      if (value?.type === 'Buffer') value = value.data
      if (value == null || typeof value === 'number') throw new TypeError('expected buffer input')
      if (typeof value === 'string') return bytesFrom(value, encoding)
      return new Uint8Array(value)
    },
    bufToString(value, encoding = 'utf8') {
      return bytesToString(value instanceof Uint8Array ? value : buffer.from(value, 'binary'), encoding)
    },
  }
  // Needle uses bracketed urlencoded fields for both form and formData
  // in the 2.12.6 preload. Its multipart option is never enabled there.
  function formFields(value, key) {
    if (Array.isArray(value)) return value.map(item => formFields(item, key + '[]')).filter(Boolean).join('&')
    if (value && typeof value === 'object') return Object.keys(value).map(name =>
      formFields(value[name], key ? key + '[' + encodeURIComponent(name) + ']' : encodeURIComponent(name))
    ).filter(Boolean).join('&')
    return key + '=' + encodeURIComponent(value ?? '')
  }

  const lx = Object.freeze({
    EVENT_NAMES, env: 'desktop', version: '2.0.0',
    get currentScriptInfo() { return currentScriptInfo },
    utils: {
      buffer,
      crypto: {
        aesEncrypt: (data, mode, key, iv) =>
          native.aes(buffer.from(data), mode, buffer.from(key), iv == null ? new Uint8Array() : buffer.from(iv)),
        rsaEncrypt: (data, key) => native.rsa(buffer.from(data), buffer.from(key)),
        randomBytes(size) {
          if (!Number.isSafeInteger(size) || size < 0) throw new RangeError('invalid random byte count')
          return native.random(size)
        },
        md5: data => native.md5(buffer.from(data)),
      },
      zlib: {
        async inflate(data) { return native.zlib(buffer.from(data), true) },
        async deflate(data) { return native.zlib(buffer.from(data), false) },
      },
    },
    async on(event, fn) {
      if (event !== 'request') throw new Error('unsupported lx.on: ' + event)
      handler = fn
    },
    async send(event, info) {
      if (event === 'updateAlert') {
        if (alerted) throw new Error('The update alert can only be called once.')
        alerted = true
        if (!info || typeof info.log !== 'string' || !info.log) throw new Error('log is required.')
        const log = info.log.length > 1024 ? info.log.slice(0, 1024) + '...' : info.log
        emit({event, id: 0, log, url: typeof info.updateUrl === 'string' && info.updateUrl.length <= 1024 ? info.updateUrl : null})
        return
      }
      if (event !== 'inited') throw new Error('unsupported lx.send: ' + event)
      if (initialized) throw new Error('Script is inited')
      initialized = true
      try {
        const sources = {}
        for (const source of ['kw', 'kg', 'tx', 'wy', 'mg', 'local']) {
          const entry = info.sources[source]
          if (entry?.type !== 'music') continue
          sources[source] = {
            type: 'music',
            actions: isArray(entry.actions)
              ? (source === 'local' ? ['musicUrl', 'lyric', 'pic'] : ['musicUrl']).filter(a => entry.actions.includes(a))
              : [],
            qualitys: source !== 'local' && isArray(entry.qualitys)
              ? ['128k', '320k', 'flac', 'flac24bit'].filter(q => entry.qualitys.includes(q))
              : [],
          }
        }
        const {rawScript, ...metadata} = currentScriptInfo
        initResult = {sources, metadata}
      } catch (err) {
        emit({event: 'error', id: 0, error: String(err)})
      }
    },
    request(url, options, callback) {
      if (typeof options === 'function') {
        callback = options
        options = {}
      } else {
        options = options && typeof options === 'object' ? options : {}
      }
      const data = options.body || options.form || options.formData
      const contentType = Object.entries(options.headers || {}).find(([k]) => k.toLowerCase() === 'content-type')?.[1]
      const isJson = typeof contentType === 'string' && /^\s*application\/json\s*(?:;|$)/i.test(contentType)
      options = {...options, body: options.body || undefined, form: options.form || undefined}
      if (data && typeof data === 'object' && !(options.body && isJson)) {
        options = {...options, body: undefined, form: formFields(data, ''), formData: undefined}
      }
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
  globalThis.window = globalThis
  globalThis.lx = lx
  return message => {
    switch (message.event) {
      case 'idle':
        idleId = message.id
        break
      case 'flush':
        if (idleId && (callbacks.size === 0 || alerted)) {
          emit({event: 'result', id: idleId, value: null})
          idleId = undefined
        }
        if (initResult) {
          emit({event: 'result', id: 0, value: initResult})
          initResult = undefined
        }
        break
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
            if (message.data.action === 'lyric') {
              if (typeof value?.lyric !== 'string' || value.lyric.length > 51200) throw new Error('invalid lyric')
              const optional = (key, limit) => typeof value[key] === 'string' && value[key].length < limit ? value[key] : null
              value = {lyric: value.lyric, tlyric: optional('tlyric', 5120), rlyric: optional('rlyric', 5120), lxlyric: optional('lxlyric', 8192)}
            } else if (typeof value !== 'string' || value.length > 8192 || !/^https?:/i.test(value)) {
              throw new Error('expected HTTP(S) ' + message.data.action)
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
