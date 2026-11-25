import {useMemo, useState, type ReactNode} from 'react';
import {Table, useTableTreeData, useTableTreeState, type TableColumn} from '@astryxdesign/core/Table';
import {TextInput} from '@astryxdesign/core/TextInput';
import {VStack} from '@astryxdesign/core/Stack';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import type {ConnectionSummary, Folder} from '../api';
import {rowLabel} from './columns';
import {buildTree, filterTree, folderRowIds, type TreeRow} from './tree';

type Props = {
  folders: Folder[];
  connections: ConnectionSummary[];
  columns: TableColumn<TreeRow>[];
  /** Shown when there is nothing at all. */
  emptyState: ReactNode;
};

/**
 * Folders and connections as an expandable table, with a search across
 * every folder. Searching shows the matches with the folders leading to
 * them opened; clearing it returns to the folders the user had open.
 */
export default function ConnectionTree({folders, connections, columns, emptyState}: Props) {
  const tree = useMemo(() => buildTree(folders, connections), [folders, connections]);
  const [query, setQuery] = useState('');
  const shown = useMemo(() => filterTree(tree, query), [tree, query]);
  const isSearching = query.trim() !== '';
  // Everything starts open, and so do folders created later; most users
  // have a handful of folders.
  const folderIds = useMemo(() => folderRowIds(tree), [tree]);
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set(folderIds));
  const [knownFolders, setKnownFolders] = useState(folderIds);
  if (knownFolders !== folderIds) {
    const added = folderIds.filter(id => !knownFolders.includes(id));
    setKnownFolders(folderIds);
    if (added.length > 0) setExpanded(ids => new Set([...ids, ...added]));
  }
  const searchExpanded = useMemo(() => new Set(folderRowIds(shown)), [shown]);

  const treeState = useTableTreeState({
    data: shown,
    idKey: 'id',
    expandedIds: isSearching ? searchExpanded : expanded,
    onExpandedIdsChange: ids => {
      if (!isSearching) setExpanded(ids);
    },
    isItemExpandable: row => row.kind === 'folder',
  });
  const treePlugin = useTableTreeData({...treeState.treeConfig, getRowLabel: rowLabel});

  if (tree.length === 0) return <>{emptyState}</>;
  return (
    <VStack gap={3}>
      <TextInput
        type="search"
        label="Search connections"
        isLabelHidden
        placeholder="Search by name, host or folder"
        startIcon="search"
        value={query}
        onChange={setQuery}
        hasClear
      />
      <Table
        data={treeState.visibleData}
        columns={columns}
        idKey="id"
        hasHover
        plugins={{tree: treePlugin}}
        emptyState={
          <EmptyState
            title="Nothing matches"
            description={`No folder or connection matches “${query.trim()}”.`}
          />
        }
      />
    </VStack>
  );
}
