import * as stylex from '@stylexjs/stylex';
import {Card} from '@astryxdesign/core/Card';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Icon} from '@astryxdesign/core/Icon';
import {Kbd} from '@astryxdesign/core/Kbd';
import {StatusDot} from '@astryxdesign/core/StatusDot';
import {Text} from '@astryxdesign/core/Text';
import {EyeIcon, SpeakerXMarkIcon} from '@heroicons/react/24/outline';
import {formatStats, type SessionStats} from './client';
import {terminalKeys} from './terminal';

const styles = stylex.create({
  topLeft: {position: 'fixed', top: 8, left: 8, pointerEvents: 'none', whiteSpace: 'pre'},
  topCenter: {position: 'fixed', top: 8, left: '50%', transform: 'translateX(-50%)', pointerEvents: 'none'},
  bottomCenter: {
    position: 'fixed',
    bottom: 8,
    left: '50%',
    transform: 'translateX(-50%)',
    pointerEvents: 'none',
  },
  topRight: {position: 'fixed', top: 8, right: 8, pointerEvents: 'none'},
  // Under the read-only badge when both show.
  secondTopRight: {top: 56},
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

/** Others connected to this session: always on screen while an administrator watches. */
export function WatchersNotice({
  administrators,
  others,
  isSecond = false,
}: {
  administrators: string[];
  others: number;
  /** Shown under another badge in the same corner. */
  isSecond?: boolean;
}) {
  const text =
    administrators.length > 0
      ? `${administrators.join(', ')} (administrator) ${administrators.length === 1 ? 'is' : 'are'} watching`
      : `${others} ${others === 1 ? 'person is' : 'people are'} connected to this session`;
  return (
    <Card padding={2} elevation="med" xstyle={[styles.topRight, isSecond && styles.secondTopRight]}>
      <HStack gap={2} vAlign="center">
        <Icon icon={EyeIcon} color={administrators.length > 0 ? 'warning' : 'secondary'} size="sm" />
        <Text>{text}</Text>
      </HStack>
    </Card>
  );
}

/** Sound waits for a gesture; the click or key press that dismisses this goes to the session as usual. */
export function AudioHint() {
  return (
    <Card padding={2} elevation="med" xstyle={styles.bottomCenter}>
      <HStack gap={2} vAlign="center">
        <Icon icon={SpeakerXMarkIcon} color="secondary" size="sm" />
        <Text>Click or press a key to turn on sound</Text>
      </HStack>
    </Card>
  );
}

/** Says that keys, clicks and touches do not reach the remote side. */
export function ReadOnlyBadge() {
  return (
    <Card padding={2} elevation="med" xstyle={styles.topRight}>
      <HStack gap={2} vAlign="center">
        <StatusDot variant="neutral" label="Read-only" />
        <Text>Read-only</Text>
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
