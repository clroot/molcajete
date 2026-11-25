import {useState} from 'react';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {List, ListItem} from '@astryxdesign/core/List';
import {Selector} from '@astryxdesign/core/Selector';
import {Button} from '@astryxdesign/core/Button';
import {IconButton} from '@astryxdesign/core/IconButton';
import {Icon} from '@astryxdesign/core/Icon';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {useToast} from '@astryxdesign/core/Toast';
import {ClipboardDocumentIcon, EyeIcon, TrashIcon, UserIcon} from '@heroicons/react/24/outline';
import {errorMessage} from '../api';
import {shareUrl, sharingApi, type Participant, type SharedSession} from './sharing';

/** Choices for how long a new link lasts, in minutes. */
const DURATIONS = [
  {value: 15, label: '15 minutes'},
  {value: 60, label: '1 hour'},
  {value: 8 * 60, label: '8 hours'},
  {value: 24 * 60, label: '24 hours'},
];

function participantDescription(p: Participant) {
  if (p.kind === 'shadow') return 'An administrator, watching';
  return p.readOnly ? 'Watching through a link' : 'Can control the session';
}

async function copy(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

type Props = {
  pageId: string;
  shared: SharedSession;
  onChange: () => void;
};

/** Who joined the session, and its share links (session menu). */
export default function SharingSection({pageId, shared, onChange}: Props) {
  const showToast = useToast();
  const [access, setAccess] = useState<'view' | 'control'>('view');
  const durations = DURATIONS.filter(d => d.value <= shared.maxLinkMinutes);
  const [minutes, setMinutes] = useState(() => durations.at(-1)?.value ?? shared.maxLinkMinutes);
  // Tokens are shown once: copying works for the links made in this page.
  const [tokens, setTokens] = useState<Record<string, string>>({});

  const copyLink = async (token: string) => {
    const copied = await copy(shareUrl(token));
    showToast(
      copied
        ? {body: 'Link copied. Anyone signed in to molcajete can open it.', autoHideDuration: 4000}
        : {body: `Copy this link: ${shareUrl(token)}`, type: 'error', isAutoHide: false},
    );
  };

  const create = async () => {
    try {
      const link = await sharingApi.createLink(pageId, access === 'view', minutes);
      if (link.token) {
        const token = link.token;
        setTokens(current => ({...current, [link.id]: token}));
        await copyLink(token);
      }
      onChange();
    } catch (err) {
      showToast({body: errorMessage(err), type: 'error'});
    }
  };

  const revoke = async (linkId: string) => {
    try {
      await sharingApi.revokeLink(pageId, linkId);
      showToast({body: 'Link revoked. Whoever joined through it was disconnected.'});
      onChange();
    } catch (err) {
      showToast({body: errorMessage(err), type: 'error'});
    }
  };

  return (
    <VStack gap={4}>
      <Heading level={3}>Sharing</Heading>
      {shared.participants.length === 0 ? (
        <Text color="secondary">Nobody else is connected.</Text>
      ) : (
        <List density="compact" edge="flush" header={<Text weight="medium">Connected</Text>} hasDividers>
          {shared.participants.map(p => (
            <ListItem
              key={p.id}
              itemKey={p.id}
              startContent={<Icon icon={p.readOnly ? EyeIcon : UserIcon} color="secondary" />}
              label={p.username}
              description={participantDescription(p)}
            />
          ))}
        </List>
      )}
      {shared.sharing && (
        <VStack gap={3}>
          {shared.links.length > 0 && (
            <List density="compact" edge="flush" header={<Text weight="medium">Links</Text>} hasDividers>
              {shared.links.map(link => {
                const token = tokens[link.id];
                return (
                  <ListItem
                    key={link.id}
                    itemKey={link.id}
                    label={link.readOnly ? 'View only' : 'Can control'}
                    description={
                      <Text type="supporting" color="secondary">
                        Until <Timestamp value={link.expiresAt} format="time" type="supporting" />
                        {link.joined > 0 ? ` · ${link.joined} joined` : ''}
                      </Text>
                    }
                    endContent={
                      <HStack gap={1}>
                        {token && (
                          <IconButton
                            label="Copy link"
                            tooltip="Copy link"
                            icon={<Icon icon={ClipboardDocumentIcon} />}
                            variant="ghost"
                            size="sm"
                            onClick={() => copyLink(token)}
                          />
                        )}
                        <IconButton
                          label="Revoke link"
                          tooltip="Revoke link"
                          icon={<Icon icon={TrashIcon} />}
                          variant="ghost"
                          size="sm"
                          clickAction={() => revoke(link.id)}
                        />
                      </HStack>
                    }
                  />
                );
              })}
            </List>
          )}
          <Selector
            label="People with the link"
            value={access}
            onChange={value => setAccess(value === 'control' ? 'control' : 'view')}
            options={[
              {value: 'view', label: 'Can watch'},
              {value: 'control', label: 'Can watch and control'},
            ]}
          />
          <Selector
            label="Link expires after"
            description="The link also stops working when you disconnect."
            value={String(minutes)}
            onChange={value => value && setMinutes(Number(value))}
            options={(durations.length > 0 ? durations : [{value: minutes, label: `${minutes} minutes`}]).map(
              d => ({value: String(d.value), label: d.label}),
            )}
          />
          <Button label="Create and copy link" variant="secondary" clickAction={create} />
        </VStack>
      )}
    </VStack>
  );
}
