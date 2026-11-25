// File transfer parameters: guacd's SFTP (SSH) and drive (RDP) settings,
// and molcajeted's size limit.
import type {ParameterField} from './protocols';

const maxFileSize: ParameterField = {
  name: 'max-file-size',
  label: 'Largest file (MiB)',
  kind: 'number',
  description: 'Bigger files are refused in both directions. Only molcajeted enforces it.',
  placeholder: 'No limit',
};

const FIELDS: Record<string, {enable: string; fields: ParameterField[]}> = {
  ssh: {
    enable: 'enable-sftp',
    fields: [
      {
        name: 'enable-sftp',
        label: 'Transfer files over SFTP',
        kind: 'switch',
        description: 'Users can browse, upload and download files with the same login as the terminal.',
      },
      {
        name: 'sftp-root-directory',
        label: 'Root folder',
        kind: 'text',
        description: 'Where browsing starts. Files outside it can still be reached in the terminal.',
        placeholder: '/',
      },
      {name: 'sftp-disable-download', label: 'Block downloads', kind: 'switch'},
      {name: 'sftp-disable-upload', label: 'Block uploads', kind: 'switch'},
      maxFileSize,
    ],
  },
  rdp: {
    enable: 'enable-drive',
    fields: [
      {
        name: 'enable-drive',
        label: 'Share a drive',
        kind: 'switch',
        description:
          'A folder on the molcajeted host shows up as a drive in the remote session, and users upload and download its files in the browser.',
      },
      {
        name: 'drive-path',
        label: 'Folder on the molcajeted host',
        kind: 'text',
        description: 'Every user of this connection shares it. Give each connection a folder of its own.',
        placeholder: '/var/lib/molcajete/drives/finance-desktop',
      },
      {
        name: 'create-drive-path',
        label: 'Create the folder if it is missing',
        kind: 'switch',
        description: 'Only the last folder of the path is created.',
      },
      {name: 'drive-name', label: 'Drive name', kind: 'text', placeholder: 'molcajete'},
      {name: 'disable-download', label: 'Block downloads to the browser', kind: 'switch'},
      {name: 'disable-upload', label: 'Block uploads from the browser', kind: 'switch'},
      maxFileSize,
    ],
  },
};

/** The file transfer fields of a protocol (none for those without). */
export function fileTransferFields(protocol: string): ParameterField[] {
  return FIELDS[protocol]?.fields ?? [];
}

/** The switch that turns file transfer on, if the protocol has one. */
export function fileTransferSwitch(protocol: string): string | null {
  return FIELDS[protocol]?.enable ?? null;
}
