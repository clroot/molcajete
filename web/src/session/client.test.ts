import {describe, expect, test} from 'bun:test';
import {formatStats, type SessionStats} from './client';

const stats: SessionStats = {
  displayWidth: 1920,
  displayHeight: 1080,
  framesPerSecond: 30,
  compositesPerSecond: 30,
  drawCallsPerSecond: 60,
  instructionsPerSecond: 120,
  bytesPerSecond: 2048,
  frameLagMs: 4,
  rttMs: 12,
  queuedOps: 0,
};

function line(text: string, name: string): string | undefined {
  return text.split('\n').find(l => l.startsWith(name));
}

describe('performance stats', () => {
  test('name the video codec and the image formats of the second', () => {
    const text = formatStats({...stats, videoCodec: 'h265', imageFormats: ['jpeg', 'png']}, 'worker');
    expect(line(text, 'video')).toBe('video     h265');
    expect(line(text, 'images')).toBe('images    jpeg, png');
  });

  test('show a dash without a video stream, without images and for a client that reports neither', () => {
    const idle = formatStats({...stats, videoCodec: null, imageFormats: []}, 'main');
    expect(line(idle, 'video')).toBe('video     -');
    expect(line(idle, 'images')).toBe('images    -');
    const older = formatStats(stats, 'main');
    expect(line(older, 'video')).toBe('video     -');
    expect(line(older, 'images')).toBe('images    -');
  });
});
