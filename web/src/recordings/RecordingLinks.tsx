import {VStack} from '@astryxdesign/core/Stack';
import {Link} from '@astryxdesign/core/Link';
import {Text} from '@astryxdesign/core/Text';
import {href} from '../route';

/** Links to a session's recordings, oldest first; a dash when it has none. */
export default function RecordingLinks({ids}: {ids: string[]}) {
  if (ids.length === 0) return <Text color="secondary">—</Text>;
  if (ids.length === 1) return <Link href={href.playback(ids[0])}>Play</Link>;
  return (
    <VStack gap={0.5}>
      {ids.map((id, index) => (
        <Link key={id} href={href.playback(id)}>
          {`Play ${index + 1}`}
        </Link>
      ))}
    </VStack>
  );
}
