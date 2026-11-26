import { rm } from 'node:fs/promises';

export async function removeFixture(dir, beforeRemove = () => {}, remove = rm) {
  await beforeRemove();
  // Windows can retain an executable's image lock briefly after stdio closes.
  await remove(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
