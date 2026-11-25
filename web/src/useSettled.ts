import {useEffect, useState} from 'react';

/** How long typing pauses before filters apply. */
const TYPING_DELAY_MS = 300;

/** `value`, once it has stopped changing for a moment. */
export function useSettled(value: string): string {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setSettled(value), TYPING_DELAY_MS);
    return () => clearTimeout(timer);
  }, [value]);
  return settled;
}
