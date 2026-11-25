import {useCallback, useEffect, useRef, useState, type RefObject} from 'react';
import {recordingFileUrl} from '../api';
import {loadClient, type MolcajeteSession, type PlaybackStatus} from '../session/client';

export type PlayerState =
  | {kind: 'starting'}
  | {kind: 'loading'; loaded: number; total: number}
  | {kind: 'failed'; message: string}
  | {
      kind: 'ready';
      isPlaying: boolean;
      isSeeking: boolean;
      hasEnded: boolean;
      time: number;
      duration: number;
      speed: number;
    };

function toState(status: PlaybackStatus): PlayerState {
  switch (status.state) {
    case 'loading':
      return {kind: 'loading', loaded: status.loaded, total: status.total};
    case 'error':
      return {kind: 'failed', message: status.message};
    default:
      return {
        kind: 'ready',
        isPlaying: status.state === 'playing',
        isSeeking: status.state === 'seeking',
        hasEnded: status.state === 'ended',
        time: status.time,
        duration: status.duration,
        speed: status.speed,
      };
  }
}

/** A wasm client in `displayRef` playing the recording `recordingId`. */
export function usePlayer(displayRef: RefObject<HTMLDivElement | null>, recordingId: string) {
  const clientRef = useRef<MolcajeteSession | null>(null);
  const [state, setState] = useState<PlayerState>({kind: 'starting'});

  useEffect(() => {
    let isCurrent = true;
    (async () => {
      try {
        const {MolcajeteSession} = await loadClient();
        const client = await MolcajeteSession.create(displayRef.current!, {playback: true});
        if (!isCurrent) {
          client.free();
          return;
        }
        clientRef.current = client;
        client.set_event_handler((type, detail) => {
          if (type === 'playback') setState(toState(detail as PlaybackStatus));
        });
        client.set_scale_mode('fit');
        client.playback('load', recordingFileUrl(recordingId));
      } catch (err) {
        if (isCurrent) setState({kind: 'failed', message: err instanceof Error ? err.message : String(err)});
      }
    })();
    return () => {
      isCurrent = false;
      clientRef.current?.free();
      clientRef.current = null;
    };
  }, [displayRef, recordingId]);

  const play = useCallback(() => clientRef.current?.playback('play'), []);
  const pause = useCallback(() => clientRef.current?.playback('pause'), []);
  const seek = useCallback((time: number) => clientRef.current?.playback('seek', time), []);
  const setSpeed = useCallback((speed: number) => clientRef.current?.playback('speed', speed), []);
  return {state, play, pause, seek, setSpeed};
}
