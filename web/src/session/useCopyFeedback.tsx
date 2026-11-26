import {useCallback, useEffect, useRef} from 'react';
import {useToast} from '@astryxdesign/core/Toast';
import {Kbd} from '@astryxdesign/core/Kbd';
import {Button} from '@astryxdesign/core/Button';
import type {SessionEvent} from './client';
import {terminalKeys} from './terminal';

const TOAST_ID = 'terminal-copy';

/**
 * Puts remote copies in the local clipboard and, in terminals, says whether
 * that worked. A selection reaches the page after a round trip to the
 * server, outside the gesture that made it, so a browser may refuse to
 * write it; the copy shortcut then still works, and without a keyboard
 * (`hasKeyboard` false) the toast has a button that copies. Returns a
 * stable handler for the client's `clipboard` and `copy` events.
 */
export function useCopyFeedback(isTerminal: boolean, hasKeyboard = true) {
  const showToast = useToast();
  const latest = useRef({showToast, isTerminal, hasKeyboard});
  useEffect(() => {
    latest.current = {showToast, isTerminal, hasKeyboard};
  }, [showToast, isTerminal, hasKeyboard]);

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
    // A tap on the toast is the gesture the browser asks for.
    const offerCopy = (text: string) => {
      const dismiss = latest.current.showToast({
        body: 'The browser did not copy the selection by itself.',
        endContent: (
          <Button
            label="Copy"
            variant="ghost"
            onClick={() => {
              dismiss();
              navigator.clipboard?.writeText(text).then(
                () => report(true, false),
                () => report(false, false),
              );
            }}
          />
        ),
        uniqueID: TOAST_ID,
        isAutoHide: true,
        autoHideDuration: 8000,
      });
    };
    if (event.type === 'clipboard') {
      const text = event.detail;
      const failed = () => {
        const {isTerminal, hasKeyboard} = latest.current;
        if (isTerminal && !hasKeyboard && navigator.clipboard) offerCopy(text);
        else report(false, true);
      };
      const write = navigator.clipboard?.writeText(text);
      if (write) write.then(() => report(true, true), failed);
      else failed();
    } else if (event.type === 'copy') {
      report(event.detail.ok, false);
    }
  }, []);
}
