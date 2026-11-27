import {describe, expect, test} from 'bun:test';
import {claimResumeId, shouldReconnect, type ResumeClaim, type ResumeEnv} from './resume';

function storage() {
  const items = new Map<string, string>();
  return {
    getItem: (key: string) => items.get(key) ?? null,
    setItem: (key: string, value: string) => void items.set(key, value),
  };
}

/** Web Locks as far as the claim uses them: exclusive locks, `ifAvailable` and `signal`. */
function lockManager(): NonNullable<ResumeEnv['locks']> {
  const held = new Map<string, Promise<unknown>>();
  const request = async (
    name: string,
    second: LockOptions | LockGrantedCallback<unknown>,
    third?: LockGrantedCallback<unknown>,
  ) => {
    const options = typeof second === 'function' ? {} : second;
    const callback = (typeof second === 'function' ? second : third)!;
    if (options.ifAvailable && held.has(name)) return callback(null);
    while (held.has(name)) {
      await Promise.race([
        held.get(name),
        new Promise((_, reject) =>
          options.signal?.addEventListener('abort', () => reject(new DOMException('', 'AbortError'))),
        ),
      ]);
    }
    const done = Promise.resolve(callback({name, mode: 'exclusive'})).finally(() => held.delete(name));
    held.set(
      name,
      done.catch(() => {}),
    );
    return done;
  };
  return {request: request as LockManager['request']};
}

/** A browser whose tabs share `local` and `locks`. */
function browser() {
  const local = storage();
  const locks = lockManager();
  let made = 0;
  const newId = () => `id-${++made}`;
  return {
    tab: (): ResumeEnv => ({local, tab: storage(), locks, newId}),
    withoutLocks: (): ResumeEnv => ({local, tab: storage(), locks: undefined, newId}),
  };
}

/** Closes the tab that holds `claim`; its lock is free a moment later. */
async function close(claim: ResumeClaim | null) {
  claim!.release();
  await new Promise(resolve => setTimeout(resolve));
}

describe('resume', () => {
  test('reconnects after a dropped tunnel, not after the session ended', () => {
    // The client's own timeout, a network drop, a gateway restart.
    for (const code of [0x0202, 0x0203, 0x0207, 0x0000]) expect(shouldReconnect(code)).toBe(true);
    // Ended by an administrator, access removed, signed out, taken over elsewhere.
    for (const code of [0x020b, 0x0303, 0x0301, 0x0209]) expect(shouldReconnect(code)).toBe(false);
  });

  test('a new tab takes the id of a closed one', async () => {
    const {tab} = browser();
    const closed = await claimResumeId('c1', tab());
    expect(closed?.id).toBe('id-1');
    await close(closed);
    expect((await claimResumeId('c1', tab()))?.id).toBe('id-1');
  });

  test('two tabs open at once have an id each', async () => {
    const {tab} = browser();
    const first = await claimResumeId('c1', tab());
    const second = await claimResumeId('c1', tab());
    expect(first?.id).toBe('id-1');
    expect(second?.id).toBe('id-2');
    // The one that was closed is the one found again.
    await close(first);
    expect((await claimResumeId('c1', tab()))?.id).toBe('id-1');
  });

  test('tabs that open together do not share an id', async () => {
    const {tab} = browser();
    const claims = await Promise.all([claimResumeId('c1', tab()), claimResumeId('c1', tab())]);
    expect(new Set(claims.map(claim => claim?.id)).size).toBe(2);
  });

  test('a reloaded tab keeps its id while other ids are free', async () => {
    const {tab} = browser();
    const reloaded = tab();
    const mine = await claimResumeId('c1', reloaded);
    const other = await claimResumeId('c1', tab());
    // The other tab's id is the latest, and free.
    await close(other);
    await close(mine);
    expect((await claimResumeId('c1', reloaded))?.id).toBe(mine!.id);
  });

  test('a duplicated tab does not take the id of the original', async () => {
    const {tab} = browser();
    const original = tab();
    const claim = await claimResumeId('c1', original);
    // The browser copies the tab's storage.
    const duplicate = tab();
    duplicate.tab.setItem('molcajete.resume.c1', claim!.id);
    expect((await claimResumeId('c1', duplicate))?.id).toBe('id-2');
  });

  test('ids belong to one connection', async () => {
    const {tab} = browser();
    await close(await claimResumeId('c1', tab()));
    expect((await claimResumeId('c2', tab()))?.id).toBe('id-2');
  });

  test('without Web Locks the id stays with the tab', async () => {
    const {withoutLocks} = browser();
    const tab = withoutLocks();
    const first = await claimResumeId('c1', tab);
    expect((await claimResumeId('c1', tab))?.id).toBe(first!.id);
    expect((await claimResumeId('c1', withoutLocks()))?.id).not.toBe(first!.id);
  });

  test('no id where storage is blocked', async () => {
    const {tab} = browser();
    const blocked = tab();
    blocked.local = {
      getItem: () => null,
      setItem: () => {
        throw new DOMException('', 'SecurityError');
      },
    };
    expect(await claimResumeId('c1', blocked)).toBeNull();
  });
});
