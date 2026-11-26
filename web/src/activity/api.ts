// Live sessions and the connection history, for administrators.
import {auditQuery, request} from '../api';

/** A session open right now (`GET /api/sessions`). */
export type LiveSession = {
  /** The tunnel id. */
  id: string;
  userId: string;
  username: string;
  connectionId: string;
  connectionName: string;
  protocol: string;
  /** The browser's address. */
  peer: string | null;
  startedAt: number;
  bytesToBrowser: number;
  bytesToGuacd: number;
  /** The latest round trip the page reported; null without client reports. */
  rttMs: number | null;
  /** The recording of *Record sessions*. */
  recordingId: string | null;
  /** Being recorded right now. */
  recording: boolean;
  /** Every recording of the session so far, oldest first. */
  recordingIds: string[];
  guacdId: string;
  /** For someone who joined another session: that session's id. */
  joinedTo: string | null;
  /** How they joined: a share link, or an administrator watching. */
  joinKind: 'link' | 'shadow' | null;
  /** Someone who joined and only watches. */
  readOnly: boolean;
};

/** Why someone else ended a session. */
export type EndReason =
  'admin' | 'user_disabled' | 'user_deleted' | 'access_revoked' | 'owner_left' | 'link_revoked';

/** A closed session (`GET /api/history`). */
export type HistoryEntry = {
  id: string;
  userId: string;
  username: string;
  connectionId: string;
  connectionName: string;
  protocol: string;
  peerAddress: string | null;
  startedAt: number;
  endedAt: number;
  /** The Guacamole status the tunnel closed with; 0 when it closed normally. */
  endStatus: number;
  endReason: EndReason | null;
  /** The administrator whose action ended it. */
  endedBy: string | null;
  bytesToBrowser: number;
  bytesToGuacd: number;
  /** The recording of *Record sessions*. */
  recordingId: string | null;
  /** Every recording of the session, oldest first, those started from its menu included. */
  recordingIds: string[];
};

/** What `GET /api/history` narrows to; empty fields do not filter. */
export type HistoryFilter = {
  /** Username or id. */
  user?: string;
  /** Connection name or id. */
  connection?: string;
  /** Started at this time or later, Unix milliseconds. */
  from?: number;
  /** Started before this time, Unix milliseconds. */
  to?: number;
};

export type HistoryPage = {entries: HistoryEntry[]; hasMore: boolean};

/** Where every session matching `filter` downloads as CSV. */
export function historyCsvUrl(filter: HistoryFilter): string {
  return new URL(`api/history?${auditQuery(filter, {format: 'csv'})}`, document.baseURI).href;
}

export const sessionsApi = {
  live: () => request<LiveSession[]>('GET', 'sessions'),
  end: (sessionId: string) => request<void>('DELETE', `sessions/${encodeURIComponent(sessionId)}`),
  /** Every session except the caller's own. */
  endAll: () => request<{ended: number}>('DELETE', 'sessions'),
  history: (filter: HistoryFilter, limit: number, offset: number) =>
    request<HistoryPage>('GET', `history?${auditQuery(filter, {limit, offset})}`),
};
