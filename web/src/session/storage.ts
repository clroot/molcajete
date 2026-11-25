// Per-browser preferences of the session page. Storage can be blocked
// (private windows, site settings); a preference then lasts for the page.

const PREFIX = 'molcajete.';

export function readPreference<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(PREFIX + key);
    return raw === null ? fallback : (JSON.parse(raw) as T);
  } catch {
    return fallback;
  }
}

export function writePreference(key: string, value: unknown): void {
  try {
    localStorage.setItem(PREFIX + key, JSON.stringify(value));
  } catch {
    // Not remembered.
  }
}
