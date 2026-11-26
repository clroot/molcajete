// `#/connect/<id>`: opens the session with the client the connection
// chose, and switches between molcajete's and IronRDP's when it allows.
import {useState} from 'react';
import {href, navigate} from '../route';
import {useConnection} from './useConnection';
import SessionPage from './SessionPage';
import IronRdpSessionPage from './IronRdpSessionPage';
import StatusOverlay from './StatusOverlay';

const leave = () => navigate(href.connections);

export default function SessionRoute({connectionId}: {connectionId: string}) {
  const {isLoaded, ...connection} = useConnection(connectionId);
  // `null` until the user switches: the connection's default.
  const [switched, setSwitched] = useState<'molcajete' | 'ironrdp' | null>(null);
  if (!isLoaded) {
    return <StatusOverlay status={{kind: 'connecting'}} connectionName={connection.name} onLeave={leave} />;
  }
  const choice = connection.rdpClient;
  const client = connection.protocol === 'rdp' ? (switched ?? choice?.client ?? 'molcajete') : 'molcajete';
  const switchTo = (to: 'molcajete' | 'ironrdp') => (choice?.canSwitch ? () => setSwitched(to) : undefined);
  return client === 'ironrdp' ? (
    <IronRdpSessionPage
      key="ironrdp"
      connectionId={connectionId}
      connectionName={connection.name}
      onLeave={leave}
      onSwitchClient={switchTo('molcajete')}
    />
  ) : (
    <SessionPage
      key="molcajete"
      connectionId={connectionId}
      connection={connection}
      onSwitchClient={connection.protocol === 'rdp' ? switchTo('ironrdp') : undefined}
    />
  );
}
