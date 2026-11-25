import type {ApiToken} from '../api';

const DAY_MS = 86_400_000;

/** How long a new token stays valid; the gateway caps it (`[api_tokens] max_lifetime_days`). */
export const LIFETIMES = [
  {value: '7', label: '7 days'},
  {value: '30', label: '30 days'},
  {value: '90', label: '90 days'},
  {value: '365', label: '1 year'},
];

/** When a token made at `now` for `days` days expires, in Unix milliseconds. */
export function expiresAt(days: string, now = Date.now()): number {
  return now + Number(days) * DAY_MS;
}

/** The addresses and ranges typed one per line (or separated by commas or spaces). */
export function parseAllowedIps(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map(part => part.trim())
    .filter(part => part !== '');
}

export function isExpired(token: ApiToken, now = Date.now()): boolean {
  return token.expiresAt <= now;
}
