// Built from the Astryx `login` page template.
import {useEffect, useState, type CSSProperties, type FormEvent} from 'react';
import {VStack} from '@astryxdesign/core/Layout';
import {Center} from '@astryxdesign/core/Center';
import {Text, Heading} from '@astryxdesign/core/Text';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Button} from '@astryxdesign/core/Button';
import {Card} from '@astryxdesign/core/Card';
import {Banner} from '@astryxdesign/core/Banner';
import {Divider} from '@astryxdesign/core/Divider';
import {Spinner} from '@astryxdesign/core/Spinner';
import {api, errorMessage, OIDC_LOGIN_URL, type AuthProviders, type Me} from '../api';
import {takeLoginError} from './loginError';
import Logo from '../shell/Logo';

// Standalone auth page paints its own body background (no host shell).
const pageStyle: CSSProperties = {
  minHeight: '100%',
  backgroundColor: 'var(--color-background-body)',
};
// Cap the column at 400px but let it shrink to fit narrow screens (Stack
// has no maxWidth prop, so it's set here).
const contentStyle: CSSProperties = {
  width: '100%',
  maxWidth: 400,
};

// Without an answer from the gateway, offer what always worked.
const PASSWORD_ONLY: AuthProviders = {passwordLogin: true, oidc: null};

// An error from single sign-on arrives in the address of a fresh page load;
// it is shown the first time the login page appears only.
let errorAtStartup = takeLoginError();

export default function LoginPage({onSignedIn}: {onSignedIn: (user: Me) => void}) {
  const [providers, setProviders] = useState<AuthProviders | null>(null);
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState(errorAtStartup);

  useEffect(() => {
    errorAtStartup = '';
    api.providers().then(setProviders, () => setProviders(PASSWORD_ONLY));
  }, []);

  const handleSignIn = async (event?: FormEvent) => {
    event?.preventDefault();
    setError('');
    if (!username || !password) {
      setError('Enter your username and password.');
      return;
    }
    setIsLoading(true);
    try {
      onSignedIn(await api.login(username, password));
    } catch (err) {
      setError(errorMessage(err));
      setIsLoading(false);
    }
  };

  return (
    <Center role="main" axis="both" padding={6} style={pageStyle}>
      <VStack gap={4} hAlign="center" style={contentStyle}>
        <VStack gap={2} hAlign="center">
          <Logo size={48} />
          <Text type="body" weight="bold" size="lg">
            molcajete
          </Text>
        </VStack>

        <Card padding={8} width="100%">
          <VStack gap={4} hAlign="stretch">
            <VStack gap={1} hAlign="center">
              <Heading level={2} accessibilityLevel={1}>
                Sign in
              </Heading>
              <Text type="body" color="secondary" size="sm">
                Remote desktops in your browser
              </Text>
            </VStack>

            {error && <Banner status="error" title={error} container="card" />}

            {providers === null ? (
              <Center axis="both" padding={4}>
                <Spinner size="lg" />
              </Center>
            ) : (
              <>
                {providers.oidc && (
                  <Button
                    label={`Sign in with ${providers.oidc.displayName}`}
                    href={OIDC_LOGIN_URL}
                    variant={providers.passwordLogin ? 'secondary' : 'primary'}
                    size="lg"
                  />
                )}
                {providers.oidc && providers.passwordLogin && <Divider label="or" />}
                {providers.passwordLogin && (
                  <form onSubmit={handleSignIn} noValidate>
                    <VStack gap={4} hAlign="stretch">
                      <TextInput
                        label="Username"
                        value={username}
                        onChange={setUsername}
                        autoComplete="username"
                        autoCapitalize="none"
                        hasAutoCorrect={false}
                        hasAutoFocus
                        size="lg"
                      />

                      <TextInput
                        label="Password"
                        value={password}
                        onChange={setPassword}
                        type="password"
                        autoComplete="current-password"
                        size="lg"
                      />

                      <Button
                        label="Sign in"
                        type="submit"
                        variant="primary"
                        size="lg"
                        isLoading={isLoading}
                      />
                    </VStack>
                  </form>
                )}
              </>
            )}
          </VStack>
        </Card>
      </VStack>
    </Center>
  );
}
