import type {AuditEntry} from '../api';

/** One line of an entry's details: a changed field, or a detail such as why a sign-in failed. */
export type ChangeRow = {field: string; before: string; after: string};

const NONE = '—';

function show(value: unknown): string {
  if (value === null || value === undefined || value === '') return NONE;
  if (typeof value === 'string') return value;
  if (Array.isArray(value)) return value.length === 0 ? NONE : value.map(show).join(', ');
  return JSON.stringify(value);
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/**
 * The rows of an entry's `changes`: `{from, to}` becomes before/after,
 * `{added, removed}` becomes what was removed/added, anything else is a
 * detail shown as the after value.
 */
export function changeRows(changes: AuditEntry['changes']): ChangeRow[] {
  if (!changes) return [];
  return Object.entries(changes).map(([field, value]) => {
    if (isObject(value) && ('from' in value || 'to' in value)) {
      return {field, before: show(value.from), after: show(value.to)};
    }
    if (isObject(value) && ('added' in value || 'removed' in value)) {
      const parts = (label: string, items: unknown) =>
        Array.isArray(items) && items.length > 0 ? `${label} ${show(items)}` : NONE;
      return {field, before: parts('Removed', value.removed), after: parts('Added', value.added)};
    }
    return {field, before: NONE, after: show(value)};
  });
}

/** Groups of actions for the filter, by their first part: `user.create` → `user`. */
export function actionGroups(actions: string[]): {prefix: string; actions: string[]}[] {
  const groups = new Map<string, string[]>();
  for (const action of actions) {
    const prefix = action.split('.')[0];
    groups.set(prefix, [...(groups.get(prefix) ?? []), action]);
  }
  return [...groups].map(([prefix, list]) => ({prefix, actions: list}));
}

/** `YYYY-MM-DD` as local midnight, in Unix milliseconds; `days` later when given. */
export function localDayStart(date: string, days = 0): number {
  const [year, month, day] = date.split('-').map(Number);
  return new Date(year, month - 1, day + days).getTime();
}
