// Getting a session back after its tunnel ended without the server ending
// it (a reload, a closed tab, a dropped network): molcajeted holds the
// session for a while, and a tunnel with the same id from the same browser
// gets it back.
//
// The browser keeps the ids it used for a connection in `localStorage`, and
// every page holds a Web Lock on the one it uses. A page that opens the
// connection takes an id no page holds (that of a closed tab, whose session
// may still be held) before it makes a new one, so two tabs on one
// connection keep a session each.

const PREFIX = 'molcajete.resume.';

/** Ids remembered per connection; more tabs than this at once still work. */
const MAX_IDS = 16;

/**
 * How long a page waits for the id its tab used before: after a reload the
 * previous document may not have let go of it yet.
 */
const OWN_ID_WAIT = 300;

/** An id this page holds until `release()`. */
export type ResumeClaim = {id: string; release: () => void};

/** What the claim needs of the browser; the tests bring their own. */
export type ResumeEnv = {
  /** Shared by the browser's tabs. */
  local: Pick<Storage, 'getItem' | 'setItem'>;
  /** This tab's, kept over a reload. */
  tab: Pick<Storage, 'getItem' | 'setItem'>;
  /** Missing in browsers without Web Locks. */
  locks: Pick<LockManager, 'request'> | undefined;
  newId: () => string;
};

function browserEnv(): ResumeEnv {
  return {
    local: localStorage,
    tab: sessionStorage,
    locks: navigator.locks,
    newId: () => crypto.randomUUID(),
  };
}

function readIds(local: ResumeEnv['local'], key: string): string[] {
  try {
    const ids: unknown = JSON.parse(local.getItem(key) ?? '[]');
    return Array.isArray(ids) ? ids.filter(id => typeof id === 'string') : [];
  } catch {
    return [];
  }
}

/**
 * Takes the lock `name` and returns what releases it, or null when another
 * page still holds it after `wait` milliseconds (0: when it is not free now).
 */
function hold(locks: Pick<LockManager, 'request'>, name: string, wait: number): Promise<(() => void) | null> {
  return new Promise(settle => {
    const controller = new AbortController();
    const timer = wait > 0 ? setTimeout(() => controller.abort(), wait) : undefined;
    locks
      .request(name, wait > 0 ? {signal: controller.signal} : {ifAvailable: true}, lock => {
        clearTimeout(timer);
        if (!lock) {
          settle(null);
          return undefined;
        }
        // Held until the page releases it or goes away.
        return new Promise<void>(release => settle(release));
      })
      .catch(() => settle(null));
  });
}

/**
 * An id for this page's session of `connectionId`, held until it is
 * released or the page goes away: the one this tab used before (a reload),
 * else the latest one no page holds (a closed tab), else a new one. Null
 * where storage is blocked: the session then ends with its tunnel.
 *
 * Without Web Locks the id stays with the tab, as `sessionStorage` does:
 * a closed tab's session is not found again.
 */
export async function claimResumeId(connectionId: string, env?: ResumeEnv): Promise<ResumeClaim | null> {
  try {
    const {local, tab, locks, newId} = env ?? browserEnv();
    const key = PREFIX + connectionId;
    const own = tab.getItem(key);
    if (!locks) {
      const id = own ?? newId();
      tab.setItem(key, id);
      return {id, release: () => {}};
    }
    // One page at a time chooses, so that no two take the same new id or
    // overwrite each other's list.
    return await locks.request(PREFIX + 'ids', async () => {
      const ids = readIds(local, key);
      const take = async (id: string, wait: number): Promise<ResumeClaim | null> => {
        const release = await hold(locks, `${key}.${id}`, wait);
        if (!release) return null;
        tab.setItem(key, id);
        // The latest first.
        local.setItem(key, JSON.stringify([id, ...ids.filter(other => other !== id)].slice(0, MAX_IDS)));
        return {id, release};
      };
      if (own) {
        const claim = await take(own, OWN_ID_WAIT);
        if (claim) return claim;
      }
      for (const id of ids) {
        if (id === own) continue;
        const claim = await take(id, 0);
        if (claim) return claim;
      }
      return take(newId(), 0);
    });
  } catch {
    return null;
  }
}

/** Forgets this browser's ids (signing out): nobody can resume what was held. */
export function forgetResumeIds(): void {
  for (const storage of [() => localStorage, () => sessionStorage]) {
    try {
      const store = storage();
      for (const key of Object.keys(store)) {
        if (key.startsWith(PREFIX)) store.removeItem(key);
      }
    } catch {
      // Nothing was stored.
    }
  }
}

/** Statuses a tunnel closes with when the session is over for good: reconnecting would not help. */
const ENDED = new Set([
  0x0204, // RESOURCE_NOT_FOUND: the connection is gone
  0x0209, // SESSION_CONFLICT: opened in another window
  0x020b, // SESSION_CLOSED: ended by an administrator
  0x0300, // CLIENT_BAD_REQUEST
  0x0301, // CLIENT_UNAUTHORIZED: signed out
  0x0303, // CLIENT_FORBIDDEN: access removed
]);

/** Waits before each attempt to reconnect, in milliseconds; after the last one the page gives up. */
export const RECONNECT_DELAYS = [500, 1000, 2000, 4000, 8000, 15000, 15000, 15000, 15000];

/** Whether a tunnel that ended with `code` (an `interrupted` event) is worth reconnecting. */
export function shouldReconnect(code: number): boolean {
  return !ENDED.has(code);
}
