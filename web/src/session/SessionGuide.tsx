import type {ReactNode} from 'react';
import {Dialog, DialogHeader} from '@astryxdesign/core/Dialog';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Button} from '@astryxdesign/core/Button';
import {Icon} from '@astryxdesign/core/Icon';
import {Kbd} from '@astryxdesign/core/Kbd';
import {Text} from '@astryxdesign/core/Text';
import {Bars3Icon} from '@heroicons/react/24/outline';

function Way({lead, children}: {lead: ReactNode; children: string}) {
  return (
    <HStack gap={3} vAlign="center">
      {lead}
      <Text>{children}</Text>
    </HStack>
  );
}

/** Shown once, on the first session in this browser: how to reach the session menu. */
export default function SessionGuide({onDismiss}: {onDismiss: () => void}) {
  return (
    <Dialog isOpen onOpenChange={isOpen => !isOpen && onDismiss()} width={420}>
      <Layout
        height="auto"
        header={
          <DialogHeader
            title="The session menu"
            subtitle="Clipboard, display scale, keyboard options and disconnecting."
          />
        }
        content={
          <LayoutContent>
            <VStack gap={4}>
              <Way lead={<Icon icon={Bars3Icon} />}>
                Click the menu button on the screen. Drag it wherever it is out of your way.
              </Way>
              <Way lead={<Kbd keys="ctrl+alt+shift" />}>Or press these three keys together.</Way>
            </VStack>
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
