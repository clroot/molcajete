import {useCallback, useEffect, useState} from 'react';
import {Center} from '@astryxdesign/core/Center';
import {Spinner} from '@astryxdesign/core/Spinner';
import {api, setUnauthorizedHandler, type User} from './api';
import {useRoute} from './route';
import AppFrame from './shell/AppFrame';
import LoginPage from './auth/LoginPage';
import ConnectionsPage from './connections/ConnectionsPage';
import UsersPage from './users/UsersPage';
import SessionPage from './session/SessionPage';

type Auth = {status: 'checking'} | {status: 'signedOut'} | {status: 'signedIn'; user: User};

const SIGNED_OUT: Auth = {status: 'signedOut'};
const signedIn = (user: User): Auth => ({status: 'signedIn', user});

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
    await api.logout().catch(() => {});
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

function SignedInApp({user, onSignOut}: {user: User; onSignOut: () => void}) {
  const route = useRoute();
  if (route.name === 'session') {
    return <SessionPage key={route.connectionId} connectionId={route.connectionId} />;
  }
  return (
    <AppFrame user={user} route={route} onSignOut={onSignOut}>
      {route.name === 'users' && user.isAdmin ? (
        <UsersPage currentUser={user} />
      ) : (
        <ConnectionsPage user={user} />
      )}
    </AppFrame>
  );
}
