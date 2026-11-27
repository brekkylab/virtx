// The declarations of `VirtxError`'s constructor, which tsc checks under strict mode: every
// argument but `message` is optional, and a call with all of them type-checks.

import { VirtxError } from '../index.js'

new VirtxError('m')
new VirtxError('refused', 503, '<html>', 'server', 'unavailable', null)
