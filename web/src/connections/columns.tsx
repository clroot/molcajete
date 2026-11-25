// Table columns shared by the administrator's and the user's connection
// lists.
import {VStack} from '@astryxdesign/core/Stack';
import {Badge} from '@astryxdesign/core/Badge';
import {Link} from '@astryxdesign/core/Link';
import {Text} from '@astryxdesign/core/Text';
import type {TableColumn} from '@astryxdesign/core/Table';
import type {ConnectionSummary} from '../api';
import {href} from '../route';
import {protocolLabel} from './protocols';

type Column = TableColumn<ConnectionSummary>;

function target(c: ConnectionSummary) {
  if (!c.hostname) return null;
  return c.port ? `${c.hostname}:${c.port}` : c.hostname;
}

export const nameColumn: Column = {
  key: 'name',
  header: 'Name',
  renderCell: c => (
    <VStack gap={0.5}>
      <Link href={href.session(c.id)} weight="medium">
        {c.name}
      </Link>
      <Text type="supporting" color="secondary">
        {c.id}
      </Text>
    </VStack>
  ),
};

export const protocolColumn: Column = {
  key: 'protocol',
  header: 'Protocol',
  renderCell: c => <Badge label={protocolLabel(c.protocol)} />,
};

export const targetColumn: Column = {
  key: 'hostname',
  header: 'Target',
  renderCell: c => {
    const value = target(c);
    return value ? <Text>{value}</Text> : <Text color="secondary">Not set</Text>;
  },
};
