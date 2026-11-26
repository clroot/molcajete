// Getting a session back after its tunnel ended without the server ending
// it (a reload, a dropped network): molcajeted holds the session for a
// while, and a tunnel with the same id from the same tab gets it back.

const PREFIX = 'molcajete.resume.';

/**
 * This tab's id for `connectionId`, made on first use. It lives in
 * `sessionStorage`, so it survives a reload and stays with the tab; another
 * tab gets a session of its own. Null where storage is blocked: the session
 * then ends with its tunnel.
 */
export function resumeId(connectionId: string): string | null {
  try {
    const key = PREFIX + connectionId;
    let id = sessionStorage.getItem(key);
    if (!id) {
      id = crypto.randomUUID();
      sessionStorage.setItem(key, id);
    }
    return id;
  } catch {
    return null;
  }
}

/** Forgets this tab's ids (signing out): nobody can resume what was held. */
export function forgetResumeIds(): void {
  try {
    for (const key of Object.keys(sessionStorage)) {
      if (key.startsWith(PREFIX)) sessionStorage.removeItem(key);
    }
  } catch {
    // Nothing was stored.
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
