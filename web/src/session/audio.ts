// The page's one AudioContext, shared by every session.
//
// Browsers create an AudioContext suspended unless the page is handling a
// user gesture. A stream's first sound arrives long after the click that
// opened the session, so the context is made (or resumed) during that click
// and handed to the client, which plays every stream in it.

let context: AudioContext | null = null;

/**
 * The page's AudioContext, created or resumed now. Call it from the click or
 * key press that starts a session; called elsewhere it still returns the
 * context, which then waits for the next gesture on the page.
 */
export function unlockAudio(): AudioContext | undefined {
  if (typeof AudioContext === 'undefined') return undefined;
  try {
    context ??= new AudioContext();
  } catch {
    return undefined;
  }
  if (context.state === 'suspended') context.resume().catch(() => {});
  return context;
}
