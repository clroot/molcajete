import {VStack} from '@astryxdesign/core/Stack';
import {Switch} from '@astryxdesign/core/Switch';
import {Banner} from '@astryxdesign/core/Banner';
import {Heading} from '@astryxdesign/core/Heading';
import {api} from '../api';
import {useApiData} from '../useApiData';
import type {RecordingChoices} from './connectionForm';

type Props = {
  recording: RecordingChoices;
  onChange: (recording: RecordingChoices) => void;
};

/** The *Record sessions* switch and what a recording holds. */
export default function RecordingSettings({recording, onChange}: Props) {
  const settings = useApiData(api.settings);
  const isOffInGateway = settings.status === 'ready' && !settings.data.recordings;
  const update = (patch: Partial<RecordingChoices>) => onChange({...recording, ...patch});

  return (
    <VStack gap={4}>
      <Heading level={3}>Recording</Heading>
      <Switch
        label="Record sessions"
        description="Every session is saved for administrators to play back under Recordings."
        value={recording.record}
        onChange={record => update({record})}
      />
      {recording.record && isOffInGateway && (
        <Banner
          status="warning"
          title="Recording is turned off in the gateway configuration"
          description="Sessions are not recorded until [recordings] enabled is true."
        />
      )}
      {recording.record && (
        <VStack gap={4}>
          <Switch
            label="Record the mouse"
            description="Shows where the pointer was and what was clicked."
            value={recording.mouse}
            onChange={mouse => update({mouse})}
          />
          <Switch
            label="Record key presses"
            description="Keeps every key typed, so the recording can show what was entered."
            value={recording.keys}
            onChange={keys => update({keys})}
          />
          {recording.keys && (
            <Banner
              status="warning"
              title="Key presses include passwords"
              description="Anything typed during the session, passwords too, can be read from the recording by every administrator."
            />
          )}
          <Switch
            label="Keep molcajete's image and video formats"
            description="Uses less bandwidth with molcajeted, but only molcajete can play the recording; the Guacamole player and guacenc cannot."
            value={recording.molcajeteFormats}
            onChange={molcajeteFormats => update({molcajeteFormats})}
          />
        </VStack>
      )}
    </VStack>
  );
}
