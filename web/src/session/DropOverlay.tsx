import * as stylex from '@stylexjs/stylex';
import {Card} from '@astryxdesign/core/Card';
import {VStack} from '@astryxdesign/core/Stack';
import {Icon} from '@astryxdesign/core/Icon';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import {ArrowUpTrayIcon} from '@heroicons/react/24/outline';

const styles = stylex.create({
  overlay: {
    position: 'fixed',
    inset: 16,
    pointerEvents: 'none',
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'center',
    opacity: 0.94,
  },
});

/** Shown while files are dragged over the session. */
export default function DropOverlay({target}: {target: string}) {
  return (
    <Card padding={6} elevation="high" xstyle={styles.overlay}>
      <VStack gap={2} hAlign="center">
        <Icon icon={ArrowUpTrayIcon} size="lg" color="secondary" />
        <Heading level={2}>Drop to upload</Heading>
        <Text color="secondary">Files go to {target}.</Text>
      </VStack>
    </Card>
  );
}
