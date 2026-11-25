// Table columns shared by the manager's and the user's connection trees.
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Badge} from '@astryxdesign/core/Badge';
import {Icon} from '@astryxdesign/core/Icon';
import {Link} from '@astryxdesign/core/Link';
import {Text} from '@astryxdesign/core/Text';
import {proportional, type TableColumn} from '@astryxdesign/core/Table';
import {FolderIcon} from '@heroicons/react/24/outline';
import type {ConnectionSummary} from '../api';
import {href} from '../route';
import {unlockAudio} from '../session/audio';
import {protocolLabel} from './protocols';
import type {TreeRow} from './tree';

type Column = TableColumn<TreeRow>;

function target(c: ConnectionSummary) {
  if (!c.hostname) return null;
  return c.port ? `${c.hostname}:${c.port}` : c.hostname;
}

/** A column that shows something for connections only. */
function connectionCell(render: (c: ConnectionSummary) => React.ReactNode) {
  return (row: TreeRow) => (row.kind === 'connection' ? render(row.connection) : null);
}

export const rowLabel = (row: TreeRow) => (row.kind === 'folder' ? row.folder.name : row.connection.name);

export const nameColumn: Column = {
  key: 'name',
  header: 'Name',
  // Wider than the others: it carries the folder indent.
  width: proportional(2, {minWidth: 240}),
  renderCell: row =>
    row.kind === 'folder' ? (
      <HStack gap={2} vAlign="center">
        <Icon icon={FolderIcon} color="secondary" size="sm" />
        <Text weight="medium">{row.folder.name}</Text>
      </HStack>
    ) : (
      <VStack gap={0.5}>
        <Link href={href.session(row.connection.id)} onClick={unlockAudio} weight="medium">
          {row.connection.name}
        </Link>
        <Text type="supporting" color="secondary">
          {row.connection.id}
        </Text>
      </VStack>
    ),
};

export const protocolColumn: Column = {
  key: 'protocol',
  header: 'Protocol',
  renderCell: connectionCell(c => <Badge label={protocolLabel(c.protocol)} />),
};

export const targetColumn: Column = {
  key: 'hostname',
  header: 'Target',
  renderCell: connectionCell(c => {
    const value = target(c);
    return value ? <Text>{value}</Text> : <Text color="secondary">Not set</Text>;
  }),
};

export const viaColumn: Column = {
  key: 'guacdHost',
  header: 'Via',
  renderCell: connectionCell(c => (
    <Text color="secondary">
      {/* Without a port the gateway uses its own [guacd] port. */}
      {c.guacdHost ? [c.guacdHost, c.guacdPort].filter(Boolean).join(':') : 'Default guacd'}
    </Text>
  )),
};
