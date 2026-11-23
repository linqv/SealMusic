/**
 * @name P0 failure cases
 * @version 1.0
 */
const sourceInfo = {sources: {wy: {type: 'music', actions: ['musicUrl'], qualitys: ['128k']}}}
lx.on('request', async ({info: {musicInfo: {fixtureOrigin, scenario}}}) => {
  switch (scenario) {
    case 'reject': throw new Error('fixture rejection')
    case 'duplicate': return lx.send('inited', sourceInfo)
    case 'loop': for (;;) {}
    case 'unsettled': return new Promise(() => {})
    case 'invalid': return 'file:///invalid'
    case 'oversize':
    case 'cancel':
    case 'timeout':
      return new Promise((resolve, reject) => {
        const url = fixtureOrigin + (scenario === 'oversize' ? '/large' : '/slow')
        const cancel = lx.request(url, {timeout: scenario === 'timeout' ? 60 : 3000}, (err, resp, body) => {
          if (!err || resp !== null || body !== null) return reject(new Error('unexpected HTTP callback'))
          reject(new Error(scenario + ' callback: ' + err.message))
        })
        if (scenario === 'cancel') setTimeout(cancel, 40)
      })
    default: throw new Error('unknown scenario')
  }
})
lx.send('inited', sourceInfo)
