// A session opened by a one-time link (`#/ticket/<ticket>`), without
// signing in: the ticket says which connection, and the tunnel uses it up.
import {useCallback} from 'react';
import {api} from '../api';
import {useApiData} from '../useApiData';
import SessionPage from './SessionPage';
import StatusOverlay from './StatusOverlay';

export default function TicketSessionPage({ticket}: {ticket: string}) {
  const info = useApiData(useCallback(() => api.ticket(ticket), [ticket]));

  switch (info.status) {
    case 'loading':
      return <StatusOverlay status={{kind: 'connecting'}} connectionName="" />;
    case 'error':
      return (
        <StatusOverlay
          status={{
            kind: 'failed',
            title: 'This link cannot be opened',
            detail: info.message,
          }}
          connectionName=""
        />
      );
    case 'ready': {
      const {connectionId, name, protocol, dpi, resizeMethod, fontSize, readOnly, disableCopy, disablePaste} =
        info.data;
      return (
        <SessionPage
          connectionId={connectionId}
          ticket={{
            ticket,
            connection: {
              name,
              protocol,
              display: {dpi, resizeMethod, fontSize},
              restrictions: {readOnly, disableCopy, disablePaste},
            },
          }}
        />
      );
    }
  }
}
