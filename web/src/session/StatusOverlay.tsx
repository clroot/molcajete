import * as stylex from '@stylexjs/stylex';
import {Card} from '@astryxdesign/core/Card';
import {Center} from '@astryxdesign/core/Center';
import {VStack, HStack} from '@astryxdesign/core/Stack';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import {Button} from '@astryxdesign/core/Button';
import {Spinner} from '@astryxdesign/core/Spinner';
import type {ConnectionStatus} from './useSessionEvents';

const styles = stylex.create({
  scrim: {position: 'fixed', inset: 0, backgroundColor: 'rgb(0 0 0 / 0.55)'},
  card: {width: '100%', maxWidth: 420},
});

type Props = {
  status: Exclude<ConnectionStatus, {kind: 'connected'}>;
  connectionName: string;
  /** Without it, there is no Reconnect button (a one-time link is used up). */
  onReconnect?: () => void;
  /** Without it, there is no way back (no connection list to go back to). */
  onLeave?: () => void;
};

/** Covers the display while connecting or reconnecting, after an error or once disconnected. */
export default function StatusOverlay({status, connectionName, onReconnect, onLeave}: Props) {
  const isConnecting = status.kind === 'connecting' || status.kind === 'reconnecting';
  return (
    <Center axis="both" padding={4} xstyle={styles.scrim}>
      <Card padding={6} xstyle={styles.card}>
        <VStack gap={4} hAlign="center">
          {isConnecting && <Spinner size="xl" />}
          <VStack gap={1} hAlign="center">
            <Heading level={1} justify="center">
              {title(status)}
            </Heading>
            <Text color="secondary" justify="center">
              {status.kind === 'failed' ? status.detail : connectionName}
            </Text>
          </VStack>
          <HStack gap={2} hAlign="center" wrap="wrap">
            {!isConnecting && onReconnect && (
              <Button label="Reconnect" variant="primary" onClick={onReconnect} />
            )}
            {onLeave && <Button label="Back to connections" variant="secondary" onClick={onLeave} />}
          </HStack>
        </VStack>
      </Card>
    </Center>
  );
}

function title(status: Props['status']) {
  switch (status.kind) {
    case 'connecting':
      return 'Connecting…';
    case 'reconnecting':
      return 'Reconnecting…';
    case 'disconnected':
      return 'Disconnected';
    case 'failed':
      return status.title;
  }
}
