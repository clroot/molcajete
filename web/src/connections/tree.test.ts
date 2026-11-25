import {describe, expect, test} from 'bun:test';
import type {ConnectionSummary, Folder} from '../api';
import {buildTree, filterTree, folderAndDescendants, folderOptions, folderPath, folderRowIds} from './tree';

const folder = (id: string, name: string, parentId: string | null = null): Folder => ({id, name, parentId});

const connection = (
  id: string,
  parentId: string | null = null,
  hostname: string | null = null,
): ConnectionSummary => ({
  id,
  name: id,
  protocol: 'ssh',
  hostname,
  port: null,
  guacdHost: null,
  guacdPort: null,
  parentId,
  dpi: null,
  resizeMethod: null,
});

const folders = [folder('db', 'Databases'), folder('prod', 'Prod', 'db'), folder('apps', 'Apps')];
const connections = [
  connection('pg', 'db', '10.0.0.5'),
  connection('mysql', 'prod'),
  connection('web'),
  connection('orphan', 'hidden'),
];

const ids = (rows: ReturnType<typeof buildTree>): unknown[] =>
  rows.map(row => (row.kind === 'folder' ? [row.id, ids(row.children)] : row.id));

describe('buildTree', () => {
  test('nests folders first, by name, and puts unknown parents at the top', () => {
    expect(ids(buildTree(folders, connections))).toEqual([
      ['folder:apps', []],
      ['folder:db', [['folder:prod', ['connection:mysql']], 'connection:pg']],
      'connection:orphan',
      'connection:web',
    ]);
  });
});

describe('filterTree', () => {
  const tree = buildTree(folders, connections);

  test('finds connections in any folder, keeping the way to them', () => {
    expect(ids(filterTree(tree, 'MYSQL'))).toEqual([['folder:db', [['folder:prod', ['connection:mysql']]]]]);
    expect(ids(filterTree(tree, '10.0.0'))).toEqual([['folder:db', ['connection:pg']]]);
  });

  test('a matching folder keeps everything in it', () => {
    expect(ids(filterTree(tree, 'prod'))).toEqual([['folder:db', [['folder:prod', ['connection:mysql']]]]]);
  });

  test('an empty query keeps everything', () => {
    expect(filterTree(tree, '  ')).toBe(tree);
  });

  test('lists every folder for expanding', () => {
    expect(folderRowIds(tree)).toEqual(['folder:apps', 'folder:db', 'folder:prod']);
  });
});

describe('folders', () => {
  test('paths and move targets', () => {
    expect(folderPath(folders, 'prod')).toBe('Databases / Prod');
    expect([...folderAndDescendants(folders, 'db')].sort()).toEqual(['db', 'prod']);
    expect(folderOptions(folders, folderAndDescendants(folders, 'db'))).toEqual([
      {value: 'apps', label: 'Apps'},
    ]);
    expect(folderOptions(folders).map(o => o.label)).toEqual(['Apps', 'Databases', 'Databases / Prod']);
  });
});
