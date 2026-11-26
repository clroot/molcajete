import {useState} from 'react';
import {VStack} from '@astryxdesign/core/Stack';
import {Button} from '@astryxdesign/core/Button';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {useToast} from '@astryxdesign/core/Toast';
import {errorMessage} from '../api';
import {sharingApi, type SessionRecording} from './sharing';

type Props = {
  pageId: string;
  recording: SessionRecording;
  onChange: () => void;
};

/** Starts and stops recording the session (session menu); nothing when it cannot be recorded. */
export default function RecordingSection({pageId, recording, onChange}: Props) {
  const showToast = useToast();
  const [isBusy, setIsBusy] = useState(false);
  // A recording that runs can be stopped even if the connection no longer allows starting one.
  const canStop = recording.active && !recording.always;
  if (!recording.always && !recording.onDemand && !canStop) return null;

  const run = async (action: () => Promise<unknown>, done: string) => {
    setIsBusy(true);
    try {
      await action();
      showToast({body: done});
      onChange();
    } catch (err) {
      showToast({body: errorMessage(err), type: 'error'});
    } finally {
      setIsBusy(false);
    }
  };

  return (
    <VStack gap={3}>
      <Heading level={3}>Recording</Heading>
      {recording.always ? (
        <Text color="secondary">Every session of this connection is recorded from start to end.</Text>
      ) : canStop ? (
        <>
          {recording.startedAt !== null && (
            <Text color="secondary">
              Recording since{' '}
              <Timestamp value={recording.startedAt} format="time" type="inherit" color="inherit" />. Others
              in the session see that it is recorded.
            </Text>
          )}
          <Button
            label="Stop recording"
            variant="destructive"
            isLoading={isBusy}
            onClick={() =>
              run(
                () => sharingApi.stopRecording(pageId),
                'Recording stopped. You can play it under Recordings.',
              )
            }
          />
        </>
      ) : (
        <>
          <Text color="secondary">
            Records this session until you stop it. You can play and download the recording under Recordings.
          </Text>
          <Button
            label="Start recording"
            isLoading={isBusy}
            onClick={() => run(() => sharingApi.startRecording(pageId), 'Recording started.')}
          />
        </>
      )}
    </VStack>
  );
}
