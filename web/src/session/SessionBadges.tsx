import * as stylex from '@stylexjs/stylex';
import {Card} from '@astryxdesign/core/Card';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Kbd} from '@astryxdesign/core/Kbd';
import {StatusDot} from '@astryxdesign/core/StatusDot';
import {Text} from '@astryxdesign/core/Text';
import {formatStats, type SessionStats} from './client';
import {terminalKeys} from './terminal';

const styles = stylex.create({
  topLeft: {position: 'fixed', top: 8, left: 8, pointerEvents: 'none', whiteSpace: 'pre'},
  topCenter: {position: 'fixed', top: 8, left: '50%', transform: 'translateX(-50%)', pointerEvents: 'none'},
  bottomLeft: {
    position: 'fixed',
    bottom: 8,
    left: 8,
    maxWidth: 'min(560px, calc(100vw - 16px))',
    pointerEvents: 'none',
  },
});

/** The target of the link under the pointer, before it is opened (like a browser's status bar). */
export function LinkStatus({url}: {url: string}) {
  return (
    <Card padding={2} elevation="med" xstyle={styles.bottomLeft}>
      <VStack gap={1}>
        <Text type="code" maxLines={1} hasTruncateTooltip={false}>
          {url}
        </Text>
        <HStack gap={1} vAlign="center">
          <Kbd keys={terminalKeys.openLink} />
          <Text type="supporting">+ click to open in a new tab</Text>
        </HStack>
      </VStack>
    </Card>
  );
}

export function UnstableNotice() {
  return (
    <Card padding={2} elevation="med" xstyle={styles.topCenter}>
      <HStack gap={2} vAlign="center">
        <StatusDot variant="warning" label="Unstable" isPulsing />
        <Text>Connection unstable</Text>
      </HStack>
    </Card>
  );
}

export function StatsPanel({stats, mode}: {stats: SessionStats; mode: string}) {
  return (
    <Card padding={2} elevation="med" xstyle={styles.topLeft}>
      <Text type="code">{formatStats(stats, mode)}</Text>
    </Card>
  );
}
