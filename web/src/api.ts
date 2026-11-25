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
  parameters: Record<string, string>;
  /** Where a new connection goes; editing does not move it. */
  parentId?: string | null;
};

export type UserInput = Partial<Grants> & {
  username?: string;
  password?: string;
  role?: Role;
  disabled?: boolean;
  groups?: string[];
};

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

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
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

export const api = {
  me: () => request<User>('GET', 'auth/me'),
  login: (username: string, password: string) => request<User>('POST', 'auth/login', {username, password}),
  logout: () => request<void>('POST', 'auth/logout'),
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
};

/** Connection managers and administrators manage connections and folders. */
export function canManageConnections(user: User) {
  return user.effectiveRole !== 'user';
}

export function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
