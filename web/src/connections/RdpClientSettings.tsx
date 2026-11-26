import {VStack} from '@astryxdesign/core/Stack';
import {Selector} from '@astryxdesign/core/Selector';
import {Switch} from '@astryxdesign/core/Switch';
import {Heading} from '@astryxdesign/core/Heading';
import type {RdpClientChoices} from './connectionForm';

type Props = {
  rdpClient: RdpClientChoices;
  /** Recorded sessions always use molcajete's client. */
  isRecorded: boolean;
  onChange: (rdpClient: RdpClientChoices) => void;
};

const RECORDED = 'Recorded sessions use molcajete. Turn off Record sessions to choose IronRDP.';

/** Which client RDP sessions open with, and whether the session menu switches between them. */
export default function RdpClientSettings({rdpClient, isRecorded, onChange}: Props) {
  const client = isRecorded ? 'molcajete' : rdpClient.client;
  return (
    <VStack gap={4}>
      <Heading level={3}>Client</Heading>
      <Selector
        label="Open sessions with"
        description="IronRDP runs RDP in the browser and molcajeted only passes the connection through: less delay, but no recording, sharing, file panel or performance stats. The stored password stays on the server either way."
        value={client}
        onChange={value => {
          if (value) onChange({...rdpClient, client: value as RdpClientChoices['client']});
        }}
        options={[
          {value: 'molcajete', label: 'molcajete'},
          {value: 'ironrdp', label: 'IronRDP'},
        ]}
        isDisabled={isRecorded}
        disabledMessage={RECORDED}
      />
      <Switch
        label="Allow switching in the session menu"
        description="Users can reconnect with the other client during a session."
        value={!isRecorded && rdpClient.canSwitch}
        onChange={canSwitch => onChange({...rdpClient, canSwitch})}
        isDisabled={isRecorded}
        disabledMessage={RECORDED}
      />
    </VStack>
  );
}
