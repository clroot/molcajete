// Thin client for the gateway's JSON API (crates/gateway/src/api.rs).

/** Fixed roles, from least to most: see `Role` in crates/gateway/src/db/users.rs. */
export type Role = 'user' | 'connection_manager' | 'admin';

export type User = {
  id: string;
  username: string;
  /** The user's own role. */
  role: Role;
  /** The highest of the user's own role and their groups' roles. */
  effectiveRole: Role;
  /** `effectiveRole === 'admin'`. */
  isAdmin: boolean;
  disabled: boolean;
  createdAt: number;
  lastLoginAt: number | null;
};

/** What a user or group is granted directly. */
export type Grants = {
  /** Connection ids. */
  connections: string[];
  /** Folder ids; each covers everything below it. */
  folders: string[];
};

export type UserDetail = User & Grants & {groups: string[]};

export type Group = {
  id: string;
  name: string;
  role: Role;
  memberCount: number;
  createdAt: number;
};

export type GroupDetail = Group & Grants & {members: string[]};

export type GroupInput = Partial<Grants> & {
  name?: string;
  role?: Role;
  members?: string[];
};

export type Folder = {
  id: string;
  name: string;
  /** null at the top level. */
  parentId: string | null;
};

/** Who is granted a connection or a folder directly. */
export type Access = {users: string[]; groups: string[]};

export type ConnectionSummary = {
  id: string;
  name: string;
  protocol: string;
  hostname: string | null;
  port: string | null;
  guacdHost: string | null;
  guacdPort: number | null;
  /** The folder it is in; null at the top level. */
  parentId: string | null;
  /** The `dpi` and `resize-method` parameters, for the session menu. */
  dpi: string | null;
  resizeMethod: string | null;
};

export type Connection = {
  id: string;
  name: string;
  protocol: string;
  guacdHost: string | null;
  guacdPort: number | null;
  /** Seconds to wait for the guacd handshake; null uses the gateway's `[guacd] handshake_timeout`. */
  guacdHandshakeTimeout?: number | null;
  /**
   * guacd parameters. Stored credentials (see `isSensitive`) never come
   * back: they are named in `storedSecrets` instead, and saving without
   * one keeps the stored value.
   */
  parameters: Record<string, string>;
  /** Credentials that have a stored value; read only. */
  storedSecrets?: string[];
  /** Stored credentials to remove when saving. */
  clearSecrets?: string[];
  /** Where a new connection goes; editing does not move it. */
  parentId?: string | null;
  /** molcajete's own settings, never sent to guacd (see `ATTRIBUTES`). */
  attributes?: Record<string, string>;
};

/** Connection attributes the gateway knows; flags are "true" or absent. */
export const ATTRIBUTES = {
  record: 'record-sessions',
  recordKeys: 'record-keys',
  recordExcludeMouse: 'record-exclude-mouse',
  recordMolcajeteFormats: 'record-molcajete-formats',
} as const;

/** A recorded session (`/api/recordings`). */
export type Recording = {
  id: string;
  connectionId: string;
  connectionName: string;
  protocol: string;
  userId: string;
  username: string;
  fileName: string;
  /** The recording holds the key presses of the session. */
  includeKeys: boolean;
  startedAt: number;
  /** Null while the session is still being recorded. */
  endedAt: number | null;
  sizeBytes: number;
};

/** One entry of the audit log (`/api/audit`). */
export type AuditEntry = {
  id: string;
  occurredAt: number;
  /** Null for someone not signed in, or the gateway itself. */
  actorId: string | null;
  /** The actor's username at the time, or the name a failed sign-in gave. */
  actorName: string | null;
  sourceAddress: string | null;
  /** Such as `user.update`; `GET /api/audit/actions` lists them. */
  action: string;
  targetType: string | null;
  targetId: string | null;
  targetName: string | null;
  /** `{field: {from, to}}`, `{field: {added, removed}}` or a detail value; secrets redacted. */
  changes: Record<string, unknown> | null;
};

/** What `GET /api/audit` narrows to; empty fields do not filter. */
export type AuditFilter = {
  actor?: string;
  /** An action, or a prefix of whole parts such as `user`. */
  action?: string;
  target?: string;
  /** Unix milliseconds, inclusive. */
  from?: number;
  /** Unix milliseconds, exclusive. */
  to?: number;
};

export type AuditPage = {entries: AuditEntry[]; hasMore: boolean};

/** The query string of `/api/audit` for `filter`, plus `extra` (paging, format). */
export function auditQuery(filter: AuditFilter, extra: Record<string, string | number> = {}): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries({...filter, ...extra})) {
    if (value !== undefined && value !== '') params.set(key, String(value));
  }
  return params.toString();
}

/** Where every entry matching `filter` downloads as CSV. */
export function auditCsvUrl(filter: AuditFilter): string {
  return new URL(`api/audit?${auditQuery(filter, {format: 'csv'})}`, document.baseURI).href;
}

/** Optional features of the gateway (`/api/settings`). */
export type GatewaySettings = {clientReports: boolean; recordings: boolean};

export type UserInput = Partial<Grants> & {
  username?: string;
  password?: string;
  role?: Role;
  disabled?: boolean;
  groups?: string[];
};

/** How people can sign in (`GET /api/auth/providers`). */
export type AuthProviders = {
  passwordLogin: boolean;
  /** Single sign-on through an OpenID Connect provider, when configured. */
  oidc: {displayName: string} | null;
};

/** Where single sign-on starts; a page navigation, not a fetch. */
export const OIDC_LOGIN_URL = 'api/auth/oidc/login';

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

let onUnauthorized: () => void = () => {};

/** Called whenever the session turns out to be gone (HTTP 401). */
export function setUnauthorizedHandler(handler: () => void) {
  onUnauthorized = handler;
}

/** Calls `api/<path>`; failures throw an {@link ApiError}. */
export async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const response = await fetch(`api/${path}`, {
    method,
    credentials: 'same-origin',
    headers: body === undefined ? undefined : {'Content-Type': 'application/json'},
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!response.ok) {
    let message = `Request failed (HTTP ${response.status}).`;
    try {
      const data = await response.json();
      if (typeof data?.error === 'string') message = data.error;
    } catch {
      // Not JSON: keep the generic message.
    }
    if (response.status === 401 && path !== 'auth/login') onUnauthorized();
    throw new ApiError(response.status, message);
  }
  if (response.status === 204) return undefined as T;
  return (await response.json()) as T;
}

const id = (value: string) => encodeURIComponent(value);

// The configuration does not change while the page is open.
let providers: Promise<AuthProviders> | null = null;
function authProviders(): Promise<AuthProviders> {
  providers ??= request<AuthProviders>('GET', 'auth/providers').catch(err => {
    providers = null;
    throw err;
  });
  return providers;
}

export const api = {
  me: () => request<User>('GET', 'auth/me'),
  login: (username: string, password: string) => request<User>('POST', 'auth/login', {username, password}),
  /** `logoutUrl`: where to sign out at the identity provider too. */
  logout: () => request<{logoutUrl?: string} | undefined>('POST', 'auth/logout'),
  providers: authProviders,
  changePassword: (currentPassword: string, newPassword: string) =>
    request<void>('PUT', 'auth/password', {currentPassword, newPassword}),

  connections: () => request<ConnectionSummary[]>('GET', 'connections'),
  connection: (connectionId: string) => request<Connection>('GET', `connections/${id(connectionId)}`),
  createConnection: (c: Connection) => request<Connection>('POST', 'connections', c),
  updateConnection: (c: Connection) => request<Connection>('PUT', `connections/${id(c.id)}`, c),
  deleteConnection: (connectionId: string) => request<void>('DELETE', `connections/${id(connectionId)}`),
  moveConnection: (connectionId: string, parentId: string | null) =>
    request<void>('PUT', `connections/${id(connectionId)}/parent`, {parentId}),
  connectionAccess: (connectionId: string) =>
    request<Access>('GET', `connections/${id(connectionId)}/access`),
  setConnectionAccess: (connectionId: string, access: Access) =>
    request<Access>('PUT', `connections/${id(connectionId)}/access`, access),

  folders: () => request<Folder[]>('GET', 'folders'),
  createFolder: (name: string, parentId: string | null) =>
    request<Folder>('POST', 'folders', {name, parentId}),
  renameFolder: (folderId: string, name: string) => request<Folder>('PUT', `folders/${id(folderId)}`, {name}),
  moveFolder: (folderId: string, parentId: string | null) =>
    request<Folder>('PUT', `folders/${id(folderId)}/parent`, {parentId}),
  deleteFolder: (folderId: string) => request<void>('DELETE', `folders/${id(folderId)}`),
  folderAccess: (folderId: string) => request<Access>('GET', `folders/${id(folderId)}/access`),
  setFolderAccess: (folderId: string, access: Access) =>
    request<Access>('PUT', `folders/${id(folderId)}/access`, access),

  users: () => request<User[]>('GET', 'users'),
  user: (userId: string) => request<UserDetail>('GET', `users/${id(userId)}`),
  createUser: (input: UserInput) => request<UserDetail>('POST', 'users', input),
  updateUser: (userId: string, input: UserInput) => request<UserDetail>('PUT', `users/${id(userId)}`, input),
  deleteUser: (userId: string) => request<void>('DELETE', `users/${id(userId)}`),

  groups: () => request<Group[]>('GET', 'groups'),
  group: (groupId: string) => request<GroupDetail>('GET', `groups/${id(groupId)}`),
  createGroup: (input: GroupInput) => request<GroupDetail>('POST', 'groups', input),
  updateGroup: (groupId: string, input: GroupInput) =>
    request<GroupDetail>('PUT', `groups/${id(groupId)}`, input),
  deleteGroup: (groupId: string) => request<void>('DELETE', `groups/${id(groupId)}`),

  recordings: () => request<Recording[]>('GET', 'recordings'),
  recording: (recordingId: string) => request<Recording>('GET', `recordings/${id(recordingId)}`),
  deleteRecording: (recordingId: string) => request<void>('DELETE', `recordings/${id(recordingId)}`),

  auditLog: (filter: AuditFilter, limit: number, offset: number) =>
    request<AuditPage>('GET', `audit?${auditQuery(filter, {limit, offset})}`),
  auditActions: () => request<string[]>('GET', 'audit/actions'),

  settings: () => request<GatewaySettings>('GET', 'settings'),
};

/** Where the file of a recording is served; `download` asks the browser to save it. */
export function recordingFileUrl(recordingId: string, download = false): string {
  const path = `api/recordings/${id(recordingId)}/file${download ? '?download=true' : ''}`;
  return new URL(path, document.baseURI).href;
}

/** Connection managers and administrators manage connections and folders. */
export function canManageConnections(user: User) {
  return user.effectiveRole !== 'user';
}

export function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
