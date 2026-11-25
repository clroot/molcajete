// File transfer state of a session: the filesystems the server exposes and
// the transfers of this page, driven by the client's events.
import {useReducer} from 'react';
import type {Filesystem, SessionEvent, Transfer} from './client';

export type FilesState = {
  filesystems: Filesystem[];
  /** Newest first. */
  transfers: Transfer[];
};

/** Finished transfers kept in the list. */
const KEEP_FINISHED = 20;

const initialState: FilesState = {filesystems: [], transfers: []};

export function reduceFiles(state: FilesState, event: SessionEvent): FilesState {
  switch (event.type) {
    case 'filesystem':
      return {
        ...state,
        filesystems: [...state.filesystems.filter(f => f.object !== event.detail.object), event.detail],
      };
    case 'filesystem-removed':
      return {...state, filesystems: state.filesystems.filter(f => f.object !== event.detail.object)};
    case 'transfer': {
      const others = state.transfers.filter(t => t.id !== event.detail.id);
      const finished = others.filter(t => t.state !== 'active').slice(0, KEEP_FINISHED - 1);
      const active = others.filter(t => t.state === 'active');
      return {...state, transfers: [event.detail, ...active, ...finished]};
    }
    case 'state':
      // A new connection exposes its filesystems again.
      if (event.detail === 'connecting') return {...state, filesystems: []};
      return state;
    default:
      return state;
  }
}

export function useFiles() {
  return useReducer(reduceFiles, initialState);
}

/** Whether downloads go to disk as they arrive (File System Access) rather than through memory. */
export function downloadsStream(): boolean {
  return typeof (window as {showSaveFilePicker?: unknown}).showSaveFilePicker === 'function';
}

/** Downloads through memory above this size get a warning first (molcajete.js BLOB_WARNING_BYTES). */
export const MEMORY_DOWNLOAD_WARNING = 1024 * 1024 * 1024;

export function formatBytes(bytes: number): string {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  return `${value.toFixed(digits)} ${units[unit]}`;
}

/** The parent of a path inside a filesystem; the root is its own parent. */
export function parentPath(path: string): string {
  const parts = path.split('/').filter(Boolean);
  parts.pop();
  return `/${parts.join('/')}`;
}

/** Path components with the path each leads to, for breadcrumbs. */
export function pathTrail(path: string): {name: string; path: string}[] {
  const parts = path.split('/').filter(Boolean);
  return parts.map((name, i) => ({name, path: `/${parts.slice(0, i + 1).join('/')}`}));
}

export function joinPath(directory: string, name: string): string {
  return `${directory.replace(/\/+$/, '')}/${name}`;
}

/** What a transfer's row says under its name. */
export function transferStatus(t: Transfer): string {
  const amount =
    t.total === null ? formatBytes(t.bytes) : `${formatBytes(t.bytes)} of ${formatBytes(t.total)}`;
  switch (t.state) {
    case 'active':
      return t.direction === 'upload' ? `Uploading · ${amount}` : `Downloading · ${amount}`;
    case 'done':
      return t.direction === 'upload'
        ? `Uploaded · ${formatBytes(t.bytes)}`
        : `Downloaded · ${formatBytes(t.bytes)}`;
    case 'cancelled':
      return 'Cancelled';
    case 'failed':
      return t.error ? `Failed: ${t.error}` : 'Failed';
  }
}
