import {useEffect} from 'react';

const SHORTCUT = ['Control', 'Alt', 'Shift'];

/** Calls `onPress` when Ctrl+Alt+Shift are held together (as in Guacamole). */
export function useMenuShortcut(onPress: () => void) {
  useEffect(() => {
    const held = new Set<string>();
    const down = (e: KeyboardEvent) => {
      held.add(e.key);
      if (SHORTCUT.every(key => held.has(key))) {
        held.clear();
        onPress();
      }
    };
    const up = (e: KeyboardEvent) => held.delete(e.key);
    const clear = () => held.clear();
    // Capture phase: the remote display must not swallow the shortcut.
    document.addEventListener('keydown', down, true);
    document.addEventListener('keyup', up, true);
    window.addEventListener('blur', clear);
    return () => {
      document.removeEventListener('keydown', down, true);
      document.removeEventListener('keyup', up, true);
      window.removeEventListener('blur', clear);
    };
  }, [onPress]);
}
