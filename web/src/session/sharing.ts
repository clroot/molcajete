// Sharing a live session: share links, who joined, and joining through a link.
import {useCallback, useEffect, useState} from 'react';
import {request} from '../api';

/** Someone who joined the session (`GET /api/my-sessions/{page}`). */
export type Participant = {
  id: string;
  username: string;
  /** `shadow`: an administrator watching. */
  kind: 'link' | 'shadow';
  readOnly: boolean;
  startedAt: number;
  linkId: string | null;
};

export type ShareLink = {
  id: string;
  readOnly: boolean;
  createdAt: number;
  expiresAt: number;
  /** Sessions joined through it right now. */
  joined: number;
  /** Only in the answer that created it. */
  token?: string;
};

export type SharedSession = {
  id: string;
  participants: Participant[];
  links: ShareLink[];
  /** Whether the gateway lets users create links. */
  sharing: boolean;
  maxLinkMinutes: number;
};

/** What a share link opens (`GET /api/join/{token}`). */
export type LinkTarget = {
  connectionName: string;
  protocol: string;
  owner: string;
  readOnly: boolean;
  expiresAt: number;
};

const page = (pageId: string) => `my-sessions/${encodeURIComponent(pageId)}`;

export const sharingApi = {
  session: (pageId: string) => request<SharedSession>('GET', page(pageId)),
  createLink: (pageId: string, readOnly: boolean, expiresInMinutes: number) =>
    request<ShareLink>('POST', `${page(pageId)}/links`, {readOnly, expiresInMinutes}),
  revokeLink: (pageId: string, linkId: string) =>
    request<void>('DELETE', `${page(pageId)}/links/${encodeURIComponent(linkId)}`),
  describeLink: (token: string) => request<LinkTarget>('GET', `join/${encodeURIComponent(token)}`),
};

/** Where a share link opens the session in this gateway's page. */
export function shareUrl(token: string): string {
  return new URL(`#/join/${encodeURIComponent(token)}`, location.href).href;
}

/** A new id for the tunnel of one page, by which its user manages sharing. */
export function newPageId(): string {
  return crypto.randomUUID();
}

/** How often who joined is checked while connected. */
const REFRESH_MS = 4000;

/**
 * Who joined the session of `pageId` and its links, checked every few
 * seconds while `isConnected` (null until known, or when the gateway does
 * not know the session: an older gateway).
 */
export function useSharedSession(pageId: string, isConnected: boolean) {
  const [shared, setShared] = useState<SharedSession | null>(null);
  const reload = useCallback(() => {
    sharingApi.session(pageId).then(setShared, () => setShared(null));
  }, [pageId]);
  useEffect(() => {
    if (!isConnected) return;
    // The tunnel registers once it is established, just before `connected`.
    const first = setTimeout(reload, 300);
    const timer = setInterval(() => {
      if (document.visibilityState === 'visible') reload();
    }, REFRESH_MS);
    return () => {
      clearTimeout(first);
      clearInterval(timer);
    };
  }, [isConnected, reload]);
  return {shared: isConnected ? shared : null, reload};
}
