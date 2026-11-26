import type {ReactNode} from 'react';
import {Dialog, DialogHeader} from '@astryxdesign/core/Dialog';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Button} from '@astryxdesign/core/Button';
import {Heading} from '@astryxdesign/core/Heading';
import {Icon} from '@astryxdesign/core/Icon';
import {Kbd} from '@astryxdesign/core/Kbd';
import {Text} from '@astryxdesign/core/Text';
import {
  ArrowsPointingOutIcon,
  ArrowsUpDownIcon,
  Bars3Icon,
  CursorArrowRaysIcon,
  HandRaisedIcon,
  LinkIcon,
} from '@heroicons/react/24/outline';
import {terminalKeys} from './terminal';

function Way({lead, children}: {lead: ReactNode; children: ReactNode}) {
  return (
    <HStack gap={3} vAlign="center">
      {lead}
      <Text>{children}</Text>
    </HStack>
  );
}

function MenuWays() {
  return (
    <VStack gap={4}>
      <Way lead={<Icon icon={Bars3Icon} />}>
        Click the menu button on the screen. Drag it wherever it is out of your way.
      </Way>
      <Way lead={<Kbd keys="ctrl+alt+shift" />}>Or press these three keys together.</Way>
    </VStack>
  );
}

function TerminalWays({hasLinks}: {hasLinks: boolean}) {
  return (
    <VStack gap={4}>
      <Way lead={<Kbd keys={terminalKeys.copy} />}>
        Copies the selected text. Selecting text with the mouse copies it too.
      </Way>
      <Way lead={<Kbd keys={terminalKeys.paste} />}>Pastes text from this computer.</Way>
      {hasLinks && (
        <Way lead={<Kbd keys={terminalKeys.openLink} />}>
          Hold this key and click a link to open it in a new tab.
        </Way>
      )}
    </VStack>
  );
}

/** A terminal used by finger: the gestures, and the bar that has the keys and the menu. */
function TouchWays({hasLinks}: {hasLinks: boolean}) {
  return (
    <VStack gap={4}>
      <Way lead={<Icon icon={CursorArrowRaysIcon} />}>Tap the terminal to open the keyboard.</Way>
      <Way lead={<Icon icon={ArrowsUpDownIcon} />}>Drag up or down to scroll through earlier output.</Way>
      <Way lead={<Icon icon={HandRaisedIcon} />}>
        Touch and hold, then drag, to select text. Lifting copies it.
      </Way>
      <Way lead={<Icon icon={ArrowsPointingOutIcon} />}>Pinch to make the text smaller or larger.</Way>
      {hasLinks && <Way lead={<Icon icon={LinkIcon} />}>Tap a link, then Open, to open it in a new tab.</Way>}
      <Way lead={<Icon icon={Bars3Icon} />}>
        The bar under the terminal has Esc, Tab, Ctrl and the arrow keys. Its last button opens the session
        menu.
      </Way>
    </VStack>
  );
}

type Props = {
  /**
   * Copy, paste and links are explained for terminals, or with `isTouch`
   * the gestures and the key bar; `null` for desktops.
   */
  terminal: {hasLinks: boolean; isTouch: boolean} | null;
  onDismiss: () => void;
};

/** Shown once in this browser, and once more on the first terminal: the session menu, copy and paste. */
export default function SessionGuide({terminal, onDismiss}: Props) {
  const isTouch = terminal?.isTouch === true;
  return (
    <Dialog isOpen onOpenChange={isOpen => !isOpen && onDismiss()} width={440}>
      <Layout
        height="auto"
        header={
          isTouch ? (
            <DialogHeader
              title="The terminal by touch"
              subtitle="Typing, scrolling, copying and the session menu."
            />
          ) : (
            <DialogHeader
              title="The session menu"
              subtitle="Clipboard, display scale, keyboard options and disconnecting."
            />
          )
        }
        content={
          <LayoutContent>
            {isTouch ? (
              <TouchWays hasLinks={terminal.hasLinks} />
            ) : terminal ? (
              <VStack gap={5}>
                <MenuWays />
                <VStack gap={3}>
                  <Heading level={3}>Copy and paste</Heading>
                  <TerminalWays hasLinks={terminal.hasLinks} />
                </VStack>
              </VStack>
            ) : (
              <MenuWays />
            )}
          </LayoutContent>
        }
        footer={
          <LayoutFooter>
            <HStack hAlign="end">
              <Button label="Got it" variant="primary" onClick={onDismiss} />
            </HStack>
          </LayoutFooter>
        }
      />
    </Dialog>
  );
}
