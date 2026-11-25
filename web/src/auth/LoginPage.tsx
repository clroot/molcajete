// Built from the Astryx `login` page template.
import {useState, type CSSProperties, type FormEvent} from 'react';
import {VStack} from '@astryxdesign/core/Layout';
import {Center} from '@astryxdesign/core/Center';
import {Text, Heading} from '@astryxdesign/core/Text';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Button} from '@astryxdesign/core/Button';
import {Card} from '@astryxdesign/core/Card';
import {Banner} from '@astryxdesign/core/Banner';
import {api, errorMessage, type User} from '../api';
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

export default function LoginPage({onSignedIn}: {onSignedIn: (user: User) => void}) {
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState('');

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
          <form onSubmit={handleSignIn} noValidate>
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

              <Button label="Sign in" type="submit" variant="primary" size="lg" isLoading={isLoading} />
            </VStack>
          </form>
        </Card>
      </VStack>
    </Center>
  );
}
