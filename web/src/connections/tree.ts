// The connection list as a tree of folders. Kept free of React so the rules
// can be tested directly.
import type {ConnectionSummary, Folder} from '../api';

export type TreeRow =
  | {kind: 'folder'; id: string; folder: Folder; children: TreeRow[]}
  | {kind: 'connection'; id: string; connection: ConnectionSummary};

export const folderRowId = (folderId: string) => `folder:${folderId}`;
export const connectionRowId = (connectionId: string) => `connection:${connectionId}`;

const byName = (a: {name: string}, b: {name: string}) => a.name.localeCompare(b.name);

/**
 * Folders first, then connections, each by name. A connection or folder
 * whose parent is not in `folders` (not visible to this user) sits at the
 * top level.
 */
export function buildTree(folders: Folder[], connections: ConnectionSummary[]): TreeRow[] {
  const known = new Set(folders.map(f => f.id));
  const parentOf = (parentId: string | null) => (parentId && known.has(parentId) ? parentId : null);
  const rowsIn = (parentId: string | null): TreeRow[] => [
    ...folders
      .filter(f => parentOf(f.parentId) === parentId)
      .sort(byName)
      .map(folder => ({
        kind: 'folder' as const,
        id: folderRowId(folder.id),
        folder,
        children: rowsIn(folder.id),
      })),
    ...connections
      .filter(c => parentOf(c.parentId) === parentId)
      .sort(byName)
      .map(connection => ({kind: 'connection' as const, id: connectionRowId(connection.id), connection})),
  ];
  return rowsIn(null);
}

function connectionMatches(c: ConnectionSummary, query: string) {
  return [c.name, c.id, c.hostname ?? '', c.protocol].some(value => value.toLowerCase().includes(query));
}

/**
 * The rows matching `query` (case-insensitive) in any folder: a connection
 * by name, id, host or protocol, a folder by name with everything in it.
 * Folders on the way to a match stay, holding only the matches.
 */
export function filterTree(rows: TreeRow[], query: string): TreeRow[] {
  const q = query.trim().toLowerCase();
  if (!q) return rows;
  return rows.flatMap((row): TreeRow[] => {
    if (row.kind === 'connection') return connectionMatches(row.connection, q) ? [row] : [];
    if (row.folder.name.toLowerCase().includes(q)) return [row];
    const children = filterTree(row.children, q);
    return children.length > 0 ? [{...row, children}] : [];
  });
}

/** Row ids of every folder in `rows`, for expanding them all. */
export function folderRowIds(rows: TreeRow[]): string[] {
  return rows.flatMap(row => (row.kind === 'folder' ? [row.id, ...folderRowIds(row.children)] : []));
}

/** "Databases / Prod" for a folder, through its parents. */
export function folderPath(folders: Folder[], folderId: string): string {
  const byId = new Map(folders.map(f => [f.id, f]));
  const names: string[] = [];
  const seen = new Set<string>();
  for (let f = byId.get(folderId); f && !seen.has(f.id); f = f.parentId ? byId.get(f.parentId) : undefined) {
    seen.add(f.id);
    names.unshift(f.name);
  }
  return names.join(' / ');
}

/** The folder and every folder inside it, which it cannot be moved into. */
export function folderAndDescendants(folders: Folder[], folderId: string): Set<string> {
  const ids = new Set([folderId]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const f of folders) {
      if (f.parentId && ids.has(f.parentId) && !ids.has(f.id)) {
        ids.add(f.id);
        grew = true;
      }
    }
  }
  return ids;
}

/** Folders as selector options labelled with their path, sorted by path. */
export function folderOptions(folders: Folder[], exclude: Set<string> = new Set()) {
  return folders
    .filter(f => !exclude.has(f.id))
    .map(f => ({value: f.id, label: folderPath(folders, f.id)}))
    .sort((a, b) => a.label.localeCompare(b.label));
}
