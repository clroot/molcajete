// Thin client for the gateway's JSON API (crates/gateway/src/api.rs).

export type User = {
  id: string;
  username: string;
  isAdmin: boolean;
  disabled: boolean;
  createdAt: number;
  lastLoginAt: number | null;
};

export type UserDetail = User & {connections: string[]};

export type ConnectionSummary = {
  id: string;
  name: string;
  protocol: string;
  hostname: string | null;
  port: string | null;
  guacdHost: string | null;
  guacdPort: number | null;
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
};

export type UserInput = {
  username?: string;
  password?: string;
  isAdmin?: boolean;
  disabled?: boolean;
  connections?: string[];
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

  users: () => request<User[]>('GET', 'users'),
  user: (userId: string) => request<UserDetail>('GET', `users/${id(userId)}`),
  createUser: (input: UserInput) => request<UserDetail>('POST', 'users', input),
  updateUser: (userId: string, input: UserInput) => request<UserDetail>('PUT', `users/${id(userId)}`, input),
  deleteUser: (userId: string) => request<void>('DELETE', `users/${id(userId)}`),
};

export function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
