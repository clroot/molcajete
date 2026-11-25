// Text for how sessions ended.
import type {EndReason, HistoryEntry} from './api';

export type Outcome = {label: string; variant: 'success' | 'warning' | 'error'};

const REASONS: Record<EndReason, string> = {
  admin: 'Ended by an administrator',
  user_disabled: 'User disabled',
  user_deleted: 'User deleted',
  access_revoked: 'Access removed',
  owner_left: 'Shared session ended',
  link_revoked: 'Share link revoked',
};

/** Guacamole statuses a session commonly closes with. */
const STATUSES: Record<number, string> = {
  0x0200: 'Server error',
  0x0202: 'Remote desktop timed out',
  0x0203: 'Remote desktop error',
  0x0207: 'Remote desktop not found',
  0x0208: 'Remote desktop unavailable',
  0x0209: 'Taken over by another session',
  0x020a: 'Idle too long',
  0x020b: 'Closed by the server',
  0x0300: 'Bad request from the browser',
  0x0301: 'Sign-in refused',
  0x0303: 'Not allowed',
  0x0308: 'Browser stopped responding',
};

/** How a session ended, for the history. */
export function outcome(entry: Pick<HistoryEntry, 'endStatus' | 'endReason'>): Outcome {
  if (entry.endReason) {
    return {label: REASONS[entry.endReason] ?? entry.endReason, variant: 'warning'};
  }
  if (entry.endStatus < 0x0100) return {label: 'Closed', variant: 'success'};
  const name = STATUSES[entry.endStatus] ?? 'Error';
  return {label: `${name} (${entry.endStatus})`, variant: 'error'};
}
