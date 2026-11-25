// Single sign-on errors: the gateway's callback sends the browser back with
// `?login_error=<code>` (see LoginError::code in crates/gateway/src/oidc).

const MESSAGES: Record<string, string> = {
  expired: 'The sign-in took too long or was started in another browser. Try again.',
  cancelled: 'Sign-in was cancelled.',
  unavailable: 'The identity provider cannot be reached right now. Try again later.',
  no_username: 'Your identity provider did not say which username to use. Ask an administrator.',
  disabled: 'Your account is disabled. Ask an administrator.',
  not_allowed: 'You do not have an account here yet. Ask an administrator.',
  username_taken: 'Your username belongs to another account here. Ask an administrator.',
  failed: 'Sign-in failed. Try again.',
};

export function loginErrorMessage(code: string): string {
  return MESSAGES[code] ?? MESSAGES.failed;
}

/**
 * Reads `login_error` from the page address and removes it, so a reload
 * does not show the error again.
 */
export function takeLoginError(): string {
  const params = new URLSearchParams(window.location.search);
  const code = params.get('login_error');
  if (code === null) return '';
  params.delete('login_error');
  const query = params.toString();
  const url = `${window.location.pathname}${query ? `?${query}` : ''}${window.location.hash}`;
  window.history.replaceState(null, '', url);
  return loginErrorMessage(code);
}
