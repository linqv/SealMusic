/**
 * @name P0 minimal
 * @version 1.0
 */
lx.on(lx.EVENT_NAMES.request, async () => 'https://example.invalid/p0.flac')
lx.send(lx.EVENT_NAMES.inited, {sources: {
  wy: {type: 'music', actions: ['musicUrl'], qualitys: ['128k']},
}})
