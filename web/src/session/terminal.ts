// Copy, paste and links in terminal sessions. The keys are handled by the
// client glue (molcajete.js, `set_terminal_shortcuts`); the page shows them.

/** Protocols drawn as a terminal, which take the copy and paste shortcuts. */
export const isTerminal = (protocol: string | null) =>
  protocol === 'ssh' || protocol === 'telnet' || protocol === 'kubernetes';

/** Terminals that report links under the pointer (molcajeted's SSH terminal). */
export const hasLinks = (protocol: string | null) => protocol === 'ssh';

/** Links the page opens: the schemes the client's own ⌘-click accepts (molcajete.js). */
export const isOpenableLink = (url: string) => /^(https?:\/\/|mailto:)./i.test(url);

const isMac = /mac/i.test(navigator.platform);

/**
 * Keys as `Kbd` takes them. On macOS ⌘ is free, so ⌘C and ⌘V; elsewhere
 * Ctrl+C and Ctrl+V stay with the terminal (interrupt, literal next).
 */
export const terminalKeys = isMac
  ? {copy: 'mod+c', paste: 'mod+v', openLink: 'mod'}
  : {copy: 'ctrl+shift+c', paste: 'ctrl+shift+v', openLink: 'ctrl'};
