import {useCallback, useEffect, useRef} from 'react';
import {useToast} from '@astryxdesign/core/Toast';
import {Kbd} from '@astryxdesign/core/Kbd';
import type {SessionEvent} from './client';
import {terminalKeys} from './terminal';

const TOAST_ID = 'terminal-copy';

/**
 * Puts remote copies in the local clipboard and, in terminals, says whether
 * that worked. A selection reaches the page after a round trip to the
 * server, outside the gesture that made it, so a browser may refuse to
 * write it; the copy shortcut then still works. Returns a stable handler
 * for the client's `clipboard` and `copy` events.
 */
export function useCopyFeedback(isTerminal: boolean) {
  const showToast = useToast();
  const latest = useRef({showToast, isTerminal});
  useEffect(() => {
    latest.current = {showToast, isTerminal};
  }, [showToast, isTerminal]);

  return useCallback((event: SessionEvent) => {
    const report = (copied: boolean, fromSelection: boolean) => {
      const {showToast, isTerminal} = latest.current;
      if (!isTerminal) return;
      if (copied) {
        showToast({body: 'Copied to the clipboard.', uniqueID: TOAST_ID, autoHideDuration: 2000});
        return;
      }
      showToast({
        body: fromSelection ? (
          <>
            The browser did not allow copying the selection. Press <Kbd keys={terminalKeys.copy} /> to copy
            it.
          </>
        ) : (
          'The browser did not allow copying the selection.'
        ),
        type: 'error',
        uniqueID: TOAST_ID,
        isAutoHide: true,
        autoHideDuration: 8000,
      });
    };
    if (event.type === 'clipboard') {
      const write = navigator.clipboard?.writeText(event.detail);
      if (write)
        write.then(
          () => report(true, true),
          () => report(false, true),
        );
      else report(false, true);
    } else if (event.type === 'copy') {
      report(event.detail.ok, false);
    }
  }, []);
}
