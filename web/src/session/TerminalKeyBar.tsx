import {useCallback, useEffect, useRef, type MouseEvent as ReactMouseEvent} from 'react';
import * as stylex from '@stylexjs/stylex';
import {Section} from '@astryxdesign/core/Section';
import {HStack} from '@astryxdesign/core/Stack';
import {ScrollableArea} from '@astryxdesign/core/ScrollableArea';
import {Button} from '@astryxdesign/core/Button';
import {IconButton} from '@astryxdesign/core/IconButton';
import {ToggleButton} from '@astryxdesign/core/ToggleButton';
import {Icon} from '@astryxdesign/core/Icon';
import {
  ArrowDownIcon,
  ArrowLeftIcon,
  ArrowRightIcon,
  ArrowUpIcon,
  Bars3Icon,
  ClipboardDocumentIcon,
  LockClosedIcon,
} from '@heroicons/react/24/outline';
import type {HeldModifiers, Hold} from './client';
import {KEYSYM, SYMBOLS, nextHold} from './keyBar';

/** How long an arrow is held before it repeats, and how often it then does. */
const REPEAT_DELAY_MS = 400;
const REPEAT_INTERVAL_MS = 80;

const styles = stylex.create({
  // Sideways panning scrolls the keys; no gesture on the bar zooms the page.
  root: {flexShrink: 0, touchAction: 'pan-x'},
  // The cut-off key at the edge shows that the strip scrolls.
  keys: {flexGrow: 1, minWidth: 0, scrollbarWidth: 'none'},
});

type Props = {
  modifiers: HeldModifiers;
  onHold: (name: keyof HeldModifiers, hold: Hold) => void;
  /** Sends a key, as an X11 keysym. */
  onKey: (keysym: number) => void;
  /** Pastes into the terminal; `null` when the connection does not allow it. */
  onPaste: (() => void) | null;
  onOpenMenu: () => void;
};

/** Ctrl or Alt: a tap holds it for the next key, a second tap locks it, a third lets go. */
function ModifierKey({
  name,
  label,
  hold,
  onHold,
}: {
  name: keyof HeldModifiers;
  label: string;
  hold: Hold;
  onHold: Props['onHold'];
}) {
  return (
    <ToggleButton
      label={hold === 'locked' ? `${label}, locked` : label}
      size="lg"
      isPressed={hold !== 'off'}
      onPressedChange={() => onHold(name, nextHold(hold))}
      icon={hold === 'locked' ? <Icon icon={LockClosedIcon} size="sm" /> : undefined}>
      {label}
    </ToggleButton>
  );
}

/** An arrow key: a tap sends it once, holding it repeats it like a key held down. */
function ArrowKey({label, icon, onPress}: {label: string; icon: typeof ArrowLeftIcon; onPress: () => void}) {
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const hasRepeated = useRef(false);
  const press = useRef(onPress);
  useEffect(() => {
    press.current = onPress;
  }, [onPress]);

  const stop = useCallback(() => {
    if (timer.current !== null) clearTimeout(timer.current);
    timer.current = null;
  }, []);
  useEffect(() => stop, [stop]);

  // Nothing is sent at the press itself: it may be the start of a swipe
  // that scrolls the keys, which the browser then cancels.
  const start = () => {
    stop();
    hasRepeated.current = false;
    const repeat = () => {
      hasRepeated.current = true;
      press.current();
      timer.current = setTimeout(repeat, REPEAT_INTERVAL_MS);
    };
    timer.current = setTimeout(repeat, REPEAT_DELAY_MS);
  };

  const onClick = () => {
    // The click that ends a hold has already sent its keys.
    if (!hasRepeated.current) onPress();
    hasRepeated.current = false;
  };

  return (
    <IconButton
      label={label}
      size="lg"
      variant="ghost"
      icon={<Icon icon={icon} />}
      onPointerDown={start}
      onPointerUp={stop}
      onPointerLeave={stop}
      onPointerCancel={stop}
      onClick={onClick}
    />
  );
}

/**
 * The keys an on-screen keyboard lacks, in a bar under the terminal: above
 * the keyboard while it is open, at the bottom of the window otherwise.
 */
export default function TerminalKeyBar({modifiers, onHold, onKey, onPaste, onOpenMenu}: Props) {
  const key = (label: string, keysym: number) => (
    <Button key={label} label={label} size="lg" variant="ghost" onClick={() => onKey(keysym)} />
  );
  return (
    <div
      {...stylex.props(styles.root)}
      // A key that takes keyboard focus from the remote screen closes the on-screen keyboard.
      onMouseDown={(event: ReactMouseEvent) => event.preventDefault()}>
      <Section variant="section" dividers={['top']} padding={2}>
        <HStack gap={2} vAlign="center">
          <ScrollableArea axis="inline" overscroll="contain" label="Terminal keys" xstyle={styles.keys}>
            <HStack gap={1} vAlign="center">
              {key('Esc', KEYSYM.escape)}
              {key('Tab', KEYSYM.tab)}
              <ModifierKey name="ctrl" label="Ctrl" hold={modifiers.ctrl} onHold={onHold} />
              <ArrowKey label="Left" icon={ArrowLeftIcon} onPress={() => onKey(KEYSYM.left)} />
              <ArrowKey label="Down" icon={ArrowDownIcon} onPress={() => onKey(KEYSYM.down)} />
              <ArrowKey label="Up" icon={ArrowUpIcon} onPress={() => onKey(KEYSYM.up)} />
              <ArrowKey label="Right" icon={ArrowRightIcon} onPress={() => onKey(KEYSYM.right)} />
              <ModifierKey name="alt" label="Alt" hold={modifiers.alt} onHold={onHold} />
              {onPaste && (
                <IconButton
                  label="Paste"
                  size="lg"
                  variant="ghost"
                  icon={<Icon icon={ClipboardDocumentIcon} />}
                  onClick={onPaste}
                />
              )}
              {SYMBOLS.map(symbol => key(symbol, symbol.codePointAt(0)!))}
              {key('Home', KEYSYM.home)}
              {key('End', KEYSYM.end)}
              {key('PgUp', KEYSYM.pageUp)}
              {key('PgDn', KEYSYM.pageDown)}
            </HStack>
          </ScrollableArea>
          <IconButton
            label="Session menu"
            size="lg"
            variant="ghost"
            icon={<Icon icon={Bars3Icon} />}
            onClick={onOpenMenu}
          />
        </HStack>
      </Section>
    </div>
  );
}
