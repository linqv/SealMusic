// Drive the release's actual preload, including Electron's contextBridge and Needle.
const { app, BrowserWindow, ipcMain } = require('electron')
const fs = require('node:fs')
const path = require('node:path')
const [archive, sourceFile, requestFile] = process.argv.slice(-3)
const fullUrl = process.argv.includes('--full-url')
const entry = path.resolve(archive)
if (JSON.parse(fs.readFileSync(path.join(entry, 'package.json'))).version !== '2.12.6') {
  throw new Error('Expected LX Music Desktop 2.12.6')
}
const script = fs.readFileSync(sourceFile, 'utf8')
const requests = JSON.parse(fs.readFileSync(requestFile, 'utf8'))
const header = script.replace(/^\uFEFF/, '').trimStart().match(/^\/\*[\s\S]*?\*\//)?.[0] ?? ''
const info = {}
for (const [key, limit] of Object.entries({name:24, description:36, version:36, author:56, homepage:1024})) {
  const value = (header.match(new RegExp('^\\s?\\*\\s?@' + key + '\\s(.+)$', 'm'))?.[1] ?? '').trim()
  info[key] = value.length > limit ? value.slice(0, limit) + '...' : value
}
info.name ||= 'user_api_' + new Date().toLocaleString()

app.whenReady().then(async () => {
  const win = new BrowserWindow({ show: false, webPreferences: {
    preload: path.join(entry, 'dist/user-api-preload.js'),
    sandbox: false, contextIsolation: true, nodeIntegration: false,
  } })
  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }))
  win.webContents.on('will-navigate', event => event.preventDefault())
  win.webContents.session.setPermissionRequestHandler((_contents, _permission, cb) => cb(false))
  let deadline
  function armDeadline() {
    clearTimeout(deadline)
    deadline = setTimeout(() => {
      console.error('LX reference deadline exceeded')
      app.exit(1)
    }, 20_000)
  }
  let id = 0
  function record(result) {
    console.log(JSON.stringify({id, ...result}))
    if (result.error || id === requests.length) return app.exit(result.error ? 1 : 0)
    armDeadline()
    win.webContents.send('userApi_request', {requestKey: String(++id), data: requests[id - 1]})
  }
  ipcMain.on('userApi_init', (_, msg) => record(msg.status ? {sources: msg.data.sources} : {error: msg.message}))
  ipcMain.on('userApi_response', (_, msg) => {
    if (msg.data.requestKey !== String(id)) return app.exit(1)
    if (!msg.status) return record({error: msg.message})
    const {action, data} = msg.data.result
    if (action === 'lyric') return record({lyric: data})
    const url = action === 'pic' ? data : data.url
    record(fullUrl ? {url} : {origin: new URL(url).origin})
  })
  const html = fs.readFileSync(path.join(entry, 'dist/userApi/renderer/user-api.html'), 'utf8')
  await win.loadURL('data:text/html;charset=UTF-8,' + encodeURIComponent(html))
  armDeadline()
  win.webContents.send('userApi_initEnv', {...info, script, proxy: {host:'', port:''}})
}).catch(() => app.exit(1))
