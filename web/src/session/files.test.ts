import {describe, expect, test} from 'bun:test';
import type {Transfer} from './client';
import {formatBytes, joinPath, parentPath, pathTrail, reduceFiles, transferStatus} from './files';

const transfer = (patch: Partial<Transfer>): Transfer => ({
  id: 't1',
  direction: 'upload',
  name: 'a.bin',
  path: '/a.bin',
  state: 'active',
  bytes: 0,
  total: 2048,
  ...patch,
});

describe('reduceFiles', () => {
  const empty = {filesystems: [], transfers: []};

  test('tracks filesystems until a new connection starts', () => {
    const fs = {object: 0, name: 'SFTP', canDownload: true, canUpload: false};
    let state = reduceFiles(empty, {type: 'filesystem', detail: fs});
    state = reduceFiles(state, {type: 'filesystem', detail: {...fs, canUpload: true}});
    expect(state.filesystems).toEqual([{...fs, canUpload: true}]);
    expect(reduceFiles(state, {type: 'state', detail: 'connecting'}).filesystems).toEqual([]);
    expect(reduceFiles(state, {type: 'filesystem-removed', detail: {object: 0}}).filesystems).toEqual([]);
  });

  test('keeps the latest state of each transfer, newest first', () => {
    let state = reduceFiles(empty, {type: 'transfer', detail: transfer({id: 't1'})});
    state = reduceFiles(state, {type: 'transfer', detail: transfer({id: 't2'})});
    state = reduceFiles(state, {type: 'transfer', detail: transfer({id: 't1', state: 'done', bytes: 2048})});
    expect(state.transfers.map(t => [t.id, t.state])).toEqual([
      ['t1', 'done'],
      ['t2', 'active'],
    ]);
  });
});

test('paths', () => {
  expect(parentPath('/a/b')).toBe('/a');
  expect(parentPath('/a')).toBe('/');
  expect(parentPath('/')).toBe('/');
  expect(pathTrail('/a/b')).toEqual([
    {name: 'a', path: '/a'},
    {name: 'b', path: '/a/b'},
  ]);
  expect(joinPath('/', 'x.txt')).toBe('/x.txt');
  expect(joinPath('/a/', 'x.txt')).toBe('/a/x.txt');
});

test('sizes and statuses', () => {
  expect(formatBytes(512)).toBe('512 B');
  expect(formatBytes(1536)).toBe('1.5 KB');
  expect(formatBytes(4 * 1024 ** 3)).toBe('4.0 GB');
  expect(transferStatus(transfer({bytes: 1024}))).toBe('Uploading · 1.0 KB of 2.0 KB');
  expect(transferStatus(transfer({state: 'failed', error: 'Permission denied'}))).toBe(
    'Failed: Permission denied',
  );
});
