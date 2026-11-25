import * as stylex from '@stylexjs/stylex';
import {Card} from '@astryxdesign/core/Card';
import {HStack} from '@astryxdesign/core/Stack';
import {StatusDot} from '@astryxdesign/core/StatusDot';
import {Text} from '@astryxdesign/core/Text';
import {formatStats, type SessionStats} from './client';

const styles = stylex.create({
  topLeft: {position: 'fixed', top: 8, left: 8, pointerEvents: 'none', whiteSpace: 'pre'},
  topCenter: {position: 'fixed', top: 8, left: '50%', transform: 'translateX(-50%)', pointerEvents: 'none'},
});

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
