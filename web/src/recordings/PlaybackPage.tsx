// Plays a session recording in the wasm client: the display fills the
// page between a header naming the session and the playback controls.
import {useCallback, useRef, useState} from 'react';
import * as stylex from '@stylexjs/stylex';
import {Layout, LayoutContent, LayoutFooter, LayoutHeader} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import {Button} from '@astryxdesign/core/Button';
import {IconButton} from '@astryxdesign/core/IconButton';
import {Slider} from '@astryxdesign/core/Slider';
import {SegmentedControl, SegmentedControlItem} from '@astryxdesign/core/SegmentedControl';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {Center} from '@astryxdesign/core/Center';
import {Card} from '@astryxdesign/core/Card';
import {Spinner} from '@astryxdesign/core/Spinner';
import {ProgressBar} from '@astryxdesign/core/ProgressBar';
import {Banner} from '@astryxdesign/core/Banner';
import {ArrowDownTrayIcon, ArrowLeftIcon, PauseIcon, PlayIcon} from '@heroicons/react/24/outline';
import {api, recordingFileUrl} from '../api';
import {useApiData} from '../useApiData';
import {href, navigate} from '../route';
import {formatClock, formatSize, SPEEDS} from './format';
import {usePlayer, type PlayerState} from './usePlayer';

const styles = stylex.create({
  page: {position: 'fixed', inset: 0},
  display: {position: 'relative', height: '100%', backgroundColor: 'black', overflow: 'hidden'},
  overlay: {position: 'absolute', inset: 0},
  card: {width: '100%', maxWidth: 420},
});

const back = () => navigate(href.recordings);

export default function PlaybackPage({recordingId}: {recordingId: string}) {
  const displayRef = useRef<HTMLDivElement>(null);
  const fetchRecording = useCallback(() => api.recording(recordingId), [recordingId]);
  const recording = useApiData(fetchRecording);
  const {state, play, pause, seek, setSpeed} = usePlayer(displayRef, recordingId);

  return (
    <div {...stylex.props(styles.page)}>
      <Layout
        height="fill"
        header={
          <LayoutHeader hasDivider padding={3}>
            <HStack gap={3} vAlign="center" hAlign="between">
              <HStack gap={3} vAlign="center">
                <IconButton
                  label="Back to recordings"
                  icon={<ArrowLeftIcon width={16} height={16} />}
                  variant="ghost"
                  onClick={back}
                />
                {recording.status === 'ready' && (
                  <VStack gap={0.5}>
                    <Heading level={1}>{recording.data.connectionName}</Heading>
                    <HStack gap={2} vAlign="center">
                      <Text color="secondary">{recording.data.username}</Text>
                      <Timestamp value={recording.data.startedAt} format="date_time" />
                      <Text color="secondary">{formatSize(recording.data.sizeBytes)}</Text>
                    </HStack>
                  </VStack>
                )}
              </HStack>
              <Button
                label="Download"
                icon={<ArrowDownTrayIcon width={16} height={16} />}
                href={recordingFileUrl(recordingId, true)}
              />
            </HStack>
          </LayoutHeader>
        }
        content={
          <LayoutContent padding={0} isScrollable={false}>
            <div {...stylex.props(styles.display)}>
              <div ref={displayRef} {...stylex.props(styles.overlay)} />
              {state.kind !== 'ready' && <PlayerOverlay state={state} />}
            </div>
          </LayoutContent>
        }
        footer={
          <LayoutFooter hasDivider padding={3}>
            <Controls state={state} onPlay={play} onPause={pause} onSeek={seek} onSpeed={setSpeed} />
          </LayoutFooter>
        }
      />
    </div>
  );
}

/** Covers the display until the recording can be played. */
function PlayerOverlay({state}: {state: Exclude<PlayerState, {kind: 'ready'}>}) {
  return (
    <Center axis="both" padding={4} xstyle={styles.overlay}>
      <Card padding={6} xstyle={styles.card}>
        {state.kind === 'failed' ? (
          <Banner status="error" title="The recording cannot be played" description={state.message} />
        ) : (
          <VStack gap={4} hAlign="center">
            {state.kind === 'loading' && state.total > 0 ? (
              <ProgressBar
                label="Loading the recording"
                value={state.loaded}
                max={state.total}
                hasValueLabel
                formatValueLabel={(value, max) => `${formatSize(value)} of ${formatSize(max)}`}
              />
            ) : (
              <>
                <Spinner size="xl" />
                <Text color="secondary">Loading the recording…</Text>
              </>
            )}
          </VStack>
        )}
      </Card>
    </Center>
  );
}

type ControlsProps = {
  state: PlayerState;
  onPlay: () => void;
  onPause: () => void;
  onSeek: (time: number) => void;
  onSpeed: (speed: number) => void;
};

function Controls({state, onPlay, onPause, onSeek, onSpeed}: ControlsProps) {
  // While the thumb is dragged it shows where it is, not where playback is.
  const [dragged, setDragged] = useState<number | null>(null);
  const ready = state.kind === 'ready' ? state : null;
  const duration = ready?.duration ?? 0;
  const time = dragged ?? ready?.time ?? 0;
  const isPlaying = ready?.isPlaying ?? false;

  return (
    <VStack gap={2}>
      <Slider
        label="Position"
        isLabelHidden
        value={time}
        min={0}
        max={Math.max(duration, 1)}
        step={100}
        formatValue={formatClock}
        isDisabled={!ready}
        width="100%"
        onChange={(value: number) => setDragged(value)}
        onChangeEnd={(value: number) => {
          setDragged(null);
          onSeek(value);
        }}
      />
      <HStack gap={4} vAlign="center" hAlign="between">
        <HStack gap={3} vAlign="center">
          <IconButton
            label={isPlaying ? 'Pause' : 'Play'}
            icon={isPlaying ? <PauseIcon width={16} height={16} /> : <PlayIcon width={16} height={16} />}
            variant="primary"
            isDisabled={!ready}
            isLoading={ready?.isSeeking}
            onClick={isPlaying ? onPause : onPlay}
          />
          <Text type="code">
            {formatClock(time)} / {formatClock(duration)}
          </Text>
        </HStack>
        <SegmentedControl
          label="Speed"
          size="sm"
          value={String(ready?.speed ?? 1)}
          isDisabled={!ready}
          onChange={value => onSpeed(Number(value))}>
          {SPEEDS.map(speed => (
            <SegmentedControlItem key={speed} value={String(speed)} label={`${speed}×`} />
          ))}
        </SegmentedControl>
      </HStack>
    </VStack>
  );
}
