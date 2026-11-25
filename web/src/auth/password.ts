/** Must match MIN_PASSWORD_LENGTH in crates/gateway/src/auth.rs. */
export const MIN_PASSWORD_LENGTH = 8;

export const PASSWORD_RULE = `At least ${MIN_PASSWORD_LENGTH} characters.`;

export function isPasswordTooShort(password: string) {
  return password !== '' && password.length < MIN_PASSWORD_LENGTH;
}
