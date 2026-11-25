import {useCallback, useEffect, useState} from 'react';
import {Center} from '@astryxdesign/core/Center';
import {Spinner} from '@astryxdesign/core/Spinner';
import {api, setUnauthorizedHandler, type Me} from './api';
import {useRoute} from './route';
import AppFrame from './shell/AppFrame';
import LoginPage from './auth/LoginPage';
import ConnectionsPage from './connections/ConnectionsPage';
import UsersPage from './users/UsersPage';
import GroupsPage from './groups/GroupsPage';
import SessionPage from './session/SessionPage';
import RecordingsPage from './recordings/RecordingsPage';
import PlaybackPage from './recordings/PlaybackPage';
import AuditPage from './audit/AuditPage';
import ActiveSessionsPage from './activity/ActiveSessionsPage';
import HistoryPage from './activity/HistoryPage';

type Auth = {status: 'checking'} | {status: 'signedOut'} | {status: 'signedIn'; user: Me};

const SIGNED_OUT: Auth = {status: 'signedOut'};
const signedIn = (user: Me): Auth => ({status: 'signedIn', user});

export default function App() {
  const [auth, setAuth] = useState<Auth>({status: 'checking'});

  useEffect(() => {
    // Any API call that finds the session gone returns to the login page.
    setUnauthorizedHandler(() => setAuth(SIGNED_OUT));
    api.me().then(
      user => setAuth(signedIn(user)),
      () => setAuth(SIGNED_OUT),
    );
  }, []);

  const signOut = useCallback(async () => {
    const result = await api.logout().catch(() => undefined);
    if (result?.logoutUrl) {
      // Signed in through the identity provider: sign out there too.
      window.location.assign(result.logoutUrl);
      return;
    }
    setAuth(SIGNED_OUT);
  }, []);

  switch (auth.status) {
    case 'checking':
      return (
        <Center axis="both" height="100%">
          <Spinner size="lg" />
        </Center>
      );
    case 'signedOut':
      return <LoginPage onSignedIn={user => setAuth(signedIn(user))} />;
    case 'signedIn':
      return <SignedInApp user={auth.user} onSignOut={signOut} />;
  }
}

function SignedInApp({user, onSignOut}: {user: Me; onSignOut: () => void}) {
  const route = useRoute();
  if (route.name === 'session') {
    return <SessionPage key={route.connectionId} connectionId={route.connectionId} />;
  }
  if (route.name === 'playback' && user.isAdmin) {
    return <PlaybackPage key={route.recordingId} recordingId={route.recordingId} />;
  }
  return (
    <AppFrame user={user} route={route} onSignOut={onSignOut}>
      {route.name === 'users' && user.isAdmin ? (
        <UsersPage currentUser={user} />
      ) : route.name === 'groups' && user.isAdmin ? (
        <GroupsPage />
      ) : route.name === 'recordings' && user.isAdmin ? (
        <RecordingsPage />
      ) : route.name === 'audit' && user.isAdmin ? (
        <AuditPage />
      ) : route.name === 'activeSessions' && user.isAdmin ? (
        <ActiveSessionsPage currentUser={user} />
      ) : route.name === 'history' && user.isAdmin ? (
        <HistoryPage />
      ) : (
        <ConnectionsPage user={user} />
      )}
    </AppFrame>
  );
}
