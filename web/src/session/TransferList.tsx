import * as stylex from '@stylexjs/stylex';
import {List, ListItem} from '@astryxdesign/core/List';
import {VStack} from '@astryxdesign/core/Stack';
import {Card} from '@astryxdesign/core/Card';
import {Text} from '@astryxdesign/core/Text';
import {Icon} from '@astryxdesign/core/Icon';
import {IconButton} from '@astryxdesign/core/IconButton';
import {ProgressBar} from '@astryxdesign/core/ProgressBar';
import {ArrowDownTrayIcon, ArrowUpTrayIcon, XMarkIcon} from '@heroicons/react/24/outline';
import type {Transfer} from './client';
import {transferStatus} from './files';

const styles = stylex.create({
  bottomRight: {
    position: 'fixed',
    bottom: 8,
    right: 8,
    width: 'min(360px, calc(100vw - 16px))',
  },
});

type Props = {
  transfers: Transfer[];
  onCancel: (id: string) => void;
};

/** Uploads and downloads with their progress; active ones can be cancelled. */
export default function TransferList({transfers, onCancel}: Props) {
  return (
    <List density="compact" edge="flush">
      {transfers.map(t => (
        <ListItem
          key={t.id}
          itemKey={t.id}
          startContent={
            <Icon
              icon={t.direction === 'upload' ? ArrowUpTrayIcon : ArrowDownTrayIcon}
              color="secondary"
              size="sm"
            />
          }
          label={t.name}
          description={
            <VStack gap={1}>
              <Text type="supporting" color="secondary">
                {transferStatus(t)}
              </Text>
              {t.state === 'active' && (
                <ProgressBar
                  label={`${t.name} progress`}
                  isLabelHidden
                  value={t.bytes}
                  max={t.total ?? 1}
                  isIndeterminate={t.total === null}
                />
              )}
            </VStack>
          }
          endContent={
            t.state === 'active' && (
              <IconButton
                label={`Cancel ${t.name}`}
                tooltip="Cancel"
                variant="ghost"
                size="sm"
                icon={<XMarkIcon width={16} height={16} />}
                onClick={() => onCancel(t.id)}
              />
            )
          }
        />
      ))}
    </List>
  );
}

/** Transfers still running, over the session while the file panel is closed. */
export function ActiveTransfers({transfers, onCancel}: Props) {
  const active = transfers.filter(t => t.state === 'active');
  if (active.length === 0) return null;
  return (
    <Card padding={2} elevation="med" xstyle={styles.bottomRight}>
      <TransferList transfers={active} onCancel={onCancel} />
    </Card>
  );
}
