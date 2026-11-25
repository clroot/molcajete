import {useCallback, useEffect, useState} from 'react';
import {Center} from '@astryxdesign/core/Center';
import {Spinner} from '@astryxdesign/core/Spinner';
import {api, setUnauthorizedHandler, type User} from './api';
import {useRoute} from './route';
import AppFrame from './AppFrame';
import LoginPage from './pages/LoginPage';
import ConnectionsPage from './pages/ConnectionsPage';
import UsersPage from './pages/UsersPage';
import SessionPage from './session/SessionPage';

export default function App() {
  // undefined while the session check is in flight, null when signed out.
  const [user, setUser] = useState<User | null | undefined>(undefined);
  const route = useRoute();

  useEffect(() => {
    setUnauthorizedHandler(() => setUser(null));
    api.me().then(setUser, () => setUser(null));
  }, []);

  const signOut = useCallback(async () => {
    await api.logout().catch(() => {});
    setUser(null);
  }, []);

  if (user === undefined) {
    return (
      <Center axis="both" height="100%">
        <Spinner size="lg" />
      </Center>
    );
  }
  if (user === null) return <LoginPage onSignedIn={setUser} />;
  if (route.name === 'session') {
    return <SessionPage key={route.connectionId} connectionId={route.connectionId} />;
  }
  return (
    <AppFrame user={user} route={route} onSignOut={signOut}>
      {route.name === 'users' && user.isAdmin ? (
        <UsersPage currentUser={user} />
      ) : (
        <ConnectionsPage user={user} />
      )}
    </AppFrame>
  );
}
