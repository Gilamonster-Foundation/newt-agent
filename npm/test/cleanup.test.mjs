import test from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter, once } from 'node:events';
import { removeFixture } from '../test-support/cleanup.mjs';

test('fixture cleanup waits for child close, not just exit', async (t) => {
  const child = new EventEmitter();
  const closed = once(child, 'close');
  const remove = t.mock.fn(async () => {});
  const cleanup = removeFixture('fixture', () => closed, remove);
  assert.equal(remove.mock.callCount(), 0, 'must not remove a running executable');
  child.emit('exit', 0, null);
  await new Promise(queueMicrotask);
  assert.equal(remove.mock.callCount(), 0, 'exit does not mean stdio is closed');
  child.emit('close', 0, null);
  await cleanup;
  assert.equal(remove.mock.callCount(), 1);
});

test('fixture cleanup bounds removal retries and preserves the locked path on failure', async (t) => {
  const path = 'fixture/native.exe';
  const error = Object.assign(new Error(`EBUSY: resource busy or locked, unlink '${path}'`), { code: 'EBUSY', path });
  const remove = t.mock.fn(async () => { throw error; });
  await assert.rejects(removeFixture('fixture', undefined, remove), (err) => err === error);
  assert.deepEqual(remove.mock.calls[0].arguments, [
    'fixture', { recursive: true, force: true, maxRetries: 5, retryDelay: 100 },
  ]);
  assert.equal(remove.mock.callCount(), 1, 'only fs.rm retries; the test is not retried');
});
