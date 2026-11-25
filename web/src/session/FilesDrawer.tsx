import {useCallback, useEffect, useState} from 'react';
import {Drawer, DrawerHeader} from '@astryxdesign/core/Drawer';
import {Layout, LayoutContent} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {List, ListItem} from '@astryxdesign/core/List';
import {Breadcrumbs, BreadcrumbItem} from '@astryxdesign/core/Breadcrumbs';
import {Banner} from '@astryxdesign/core/Banner';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {FileInput} from '@astryxdesign/core/FileInput';
import {Heading} from '@astryxdesign/core/Heading';
import {Icon} from '@astryxdesign/core/Icon';
import {IconButton} from '@astryxdesign/core/IconButton';
import {Selector} from '@astryxdesign/core/Selector';
import {Spinner} from '@astryxdesign/core/Spinner';
import {AlertDialog} from '@astryxdesign/core/AlertDialog';
import {ArrowDownTrayIcon, ArrowPathIcon, DocumentIcon, FolderIcon} from '@heroicons/react/24/outline';
import type {FileEntry, Filesystem, MolcajeteSession, Transfer} from './client';
import {
  MEMORY_DOWNLOAD_WARNING,
  downloadsStream,
  formatBytes,
  joinPath,
  parentPath,
  pathTrail,
} from './files';
import TransferList from './TransferList';
import {useApiData, type ApiData} from '../useApiData';

type Listing = ApiData<FileEntry[]>;

type Props = {
  isOpen: boolean;
  onOpenChange: (isOpen: boolean) => void;
  filesystems: Filesystem[];
  transfers: Transfer[];
  client: () => MolcajeteSession | null;
};

/** Browses the session's filesystems, uploads into the folder shown and downloads files. */
export default function FilesDrawer({isOpen, onOpenChange, filesystems, transfers, client}: Props) {
  return (
    <Drawer isOpen={isOpen} onOpenChange={onOpenChange} label="Files" width={440}>
      {/* Mounted while open, so that a folder is listed when the panel opens. */}
      {isOpen && (
        <FilesPanel
          onOpenChange={onOpenChange}
          filesystems={filesystems}
          transfers={transfers}
          client={client}
        />
      )}
    </Drawer>
  );
}

function FilesPanel({onOpenChange, filesystems, transfers, client}: Omit<Props, 'isOpen'>) {
  const [object, setObject] = useState<number | null>(null);
  const filesystem = filesystems.find(f => f.object === object) ?? filesystems[0] ?? null;
  const [path, setPath] = useState('/');
  const [largeDownload, setLargeDownload] = useState<FileEntry | null>(null);

  const fetchListing = useCallback(async () => {
    const session = client();
    if (!session || !filesystem) return [];
    return session.list(filesystem.object, path);
  }, [client, filesystem, path]);
  const listing = useApiData(fetchListing);
  const load = listing.reload;

  // Show what finished uploading into this folder.
  const uploadedHere = transfers.filter(
    t => t.direction === 'upload' && t.state === 'done' && parentPath(t.path) === path,
  ).length;
  useEffect(() => {
    if (uploadedHere > 0) load();
  }, [uploadedHere, load]);

  const startDownload = (entry: FileEntry) => {
    if (!filesystem) return;
    void client()?.download(filesystem.object, entry.path, {name: entry.name, size: entry.size});
  };
  const download = (entry: FileEntry) => {
    if (!downloadsStream() && (entry.size ?? 0) > MEMORY_DOWNLOAD_WARNING) setLargeDownload(entry);
    else startDownload(entry);
  };
  const upload = (files: File | File[] | null) => {
    const session = client();
    if (!session || !filesystem || !files) return;
    for (const file of Array.isArray(files) ? files : [files]) {
      session.upload(file, {object: filesystem.object, path: joinPath(path, file.name)});
    }
  };

  return (
    <>
      <Layout
        header={
          <DrawerHeader
            title="Files"
            endContent={
              filesystem && (
                <IconButton
                  label="Refresh"
                  tooltip="Refresh"
                  variant="ghost"
                  icon={<ArrowPathIcon width={16} height={16} />}
                  onClick={load}
                />
              )
            }
            onOpenChange={onOpenChange}
          />
        }
        content={
          <LayoutContent>
            {!filesystem ? (
              <EmptyState
                title="No files"
                description="This connection does not share files. An administrator can turn on SFTP or a drive for it."
              />
            ) : (
              <VStack gap={5}>
                {filesystems.length > 1 && (
                  <Selector
                    label="Location"
                    value={String(filesystem.object)}
                    onChange={value => {
                      setObject(Number(value));
                      setPath('/');
                    }}
                    options={filesystems.map(f => ({value: String(f.object), label: f.name}))}
                  />
                )}
                <Breadcrumbs label="Folder">
                  <BreadcrumbItem onClick={() => setPath('/')} isCurrent={path === '/'}>
                    {filesystem.name}
                  </BreadcrumbItem>
                  {pathTrail(path).map(step => (
                    <BreadcrumbItem
                      key={step.path}
                      onClick={() => setPath(step.path)}
                      isCurrent={step.path === path}>
                      {step.name}
                    </BreadcrumbItem>
                  ))}
                </Breadcrumbs>
                {filesystem.canUpload && (
                  <FileInput
                    label="Upload to this folder"
                    isLabelHidden
                    placeholder="Upload to this folder"
                    isMultiple
                    value={null}
                    onChange={upload}
                  />
                )}
                <FolderContents
                  listing={listing}
                  canDownload={filesystem.canDownload}
                  onOpen={entry => setPath(entry.path)}
                  onDownload={download}
                />
                {transfers.length > 0 && (
                  <VStack gap={2}>
                    <Heading level={3}>Transfers</Heading>
                    <TransferList transfers={transfers} onCancel={id => client()?.cancel_transfer(id)} />
                  </VStack>
                )}
              </VStack>
            )}
          </LayoutContent>
        }
      />
      <AlertDialog
        isOpen={largeDownload !== null}
        onOpenChange={open => !open && setLargeDownload(null)}
        title="Download through memory?"
        description={`This browser cannot write a download straight to disk, so the whole file (${formatBytes(
          largeDownload?.size ?? 0,
        )}) is kept in memory until it is saved. A file this large may slow down or crash the tab. Chrome and Edge write it to disk as it arrives.`}
        actionLabel="Download anyway"
        actionVariant="primary"
        onAction={() => {
          if (largeDownload) startDownload(largeDownload);
          setLargeDownload(null);
        }}
      />
    </>
  );
}

type FolderProps = {
  listing: Listing;
  canDownload: boolean;
  onOpen: (entry: FileEntry) => void;
  onDownload: (entry: FileEntry) => void;
};

function FolderContents({listing, canDownload, onOpen, onDownload}: FolderProps) {
  if (listing.status === 'loading') {
    return (
      <HStack hAlign="center" paddingBlock={4}>
        <Spinner aria-label="Loading the folder" />
      </HStack>
    );
  }
  if (listing.status === 'error') {
    return <Banner status="error" title="The folder could not be opened" description={listing.message} />;
  }
  if (listing.data.length === 0) {
    return <EmptyState title="This folder is empty" />;
  }
  return (
    <List density="compact" hasDividers edge="flush">
      {listing.data.map(entry =>
        entry.type === 'directory' ? (
          <ListItem
            key={entry.path}
            itemKey={entry.path}
            startContent={<Icon icon={FolderIcon} color="secondary" size="sm" />}
            label={entry.name}
            onClick={() => onOpen(entry)}
          />
        ) : (
          <ListItem
            key={entry.path}
            itemKey={entry.path}
            startContent={<Icon icon={DocumentIcon} color="secondary" size="sm" />}
            label={entry.name}
            description={entry.size === null ? undefined : formatBytes(entry.size)}
            endContent={
              canDownload && (
                <IconButton
                  label={`Download ${entry.name}`}
                  tooltip="Download"
                  variant="ghost"
                  size="sm"
                  icon={<ArrowDownTrayIcon width={16} height={16} />}
                  onClick={() => onDownload(entry)}
                />
              )
            }
          />
        ),
      )}
    </List>
  );
}
