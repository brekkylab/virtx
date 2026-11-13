// A sandbox's ports as `setInbound` leaves them, against a client that answers as virtx cloud
// does: no server, no machine.

import assert from 'node:assert/strict'
import { test } from 'node:test'

import virtx from '../index.js'

const { Sandbox } = virtx

/** A sandbox whose client answers `setInbound` with `address`, as virtx cloud does. */
function sandbox(address, ports) {
  const client = {
    async _call(name, { port }) {
      assert.equal(name, 'virtx_client_set_sandbox_port_inbound')
      return address === undefined ? { port, rules: [] } : { port, rules: [], address }
    },
  }
  return new Sandbox(client, { id: 'sbx_one', ports })
}

test('the address setInbound answers is kept on its port (issue #184)', async () => {
  const given = sandbox('203.0.113.7:30001', [
    { port: 8000, url: 'wss://p/8000' },
    { port: 9000, url: 'wss://p/9000' },
  ])
  await given.setInbound(8000, [{ source: '203.0.113.0/24' }])
  assert.deepEqual(
    given.ports.map((p) => p.address),
    ['203.0.113.7:30001', undefined],
  )

  // A rule set that answers none takes it away.
  const taken = sandbox(undefined, [
    { port: 8000, url: 'wss://p/8000', address: '203.0.113.7:30001' },
  ])
  await taken.setInbound(8000, [])
  assert.equal(taken.ports[0].address, undefined)
})
