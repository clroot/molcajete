import {VStack} from '@astryxdesign/core/Stack';
import {MultiSelector} from '@astryxdesign/core/MultiSelector';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import type {ConnectionSummary, Folder, Grants} from '../api';
import {protocolLabel} from '../connections/protocols';
import {folderOptions, folderPath} from '../connections/tree';

type Props = {
  /** Who is being granted, e.g. "this user". */
  subject: string;
  grants: Grants;
  folders: Folder[];
  connections: ConnectionSummary[];
  onChange: (grants: Grants) => void;
};

/** Folders and connections granted directly to a user or a group. */
export default function GrantFields({subject, grants, folders, connections, onChange}: Props) {
  const connectionOptions = connections.map(c => ({
    value: c.id,
    label: c.parentId ? `${folderPath(folders, c.parentId)} / ${c.name}` : c.name,
    description: [protocolLabel(c.protocol), c.hostname].filter(Boolean).join(' · '),
  }));
  return (
    <VStack gap={4}>
      <VStack gap={1}>
        <Heading level={3}>Access</Heading>
        <Text color="secondary">The folders and connections {subject} can open.</Text>
      </VStack>
      <MultiSelector
        label="Folders"
        description="Covers every connection in the folder and in the folders inside it."
        value={grants.folders}
        onChange={ids => onChange({...grants, folders: ids})}
        options={folderOptions(folders)}
        triggerDisplay="badges"
        placeholder="No folders"
        hasSearch
        searchPlaceholder="Search folders"
        emptyText="There are no folders yet."
      />
      <MultiSelector
        label="Connections"
        value={grants.connections}
        onChange={ids => onChange({...grants, connections: ids})}
        options={connectionOptions.sort((a, b) => a.label.localeCompare(b.label))}
        triggerDisplay="badges"
        placeholder="No connections"
        hasSearch
        searchPlaceholder="Search connections"
        emptyText="There are no connections yet."
      />
    </VStack>
  );
}
