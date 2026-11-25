import {Drawer, DrawerHeader} from '@astryxdesign/core/Drawer';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {TextArea} from '@astryxdesign/core/TextArea';
import {Selector} from '@astryxdesign/core/Selector';
import {Switch} from '@astryxdesign/core/Switch';
import {Button} from '@astryxdesign/core/Button';
import {Kbd} from '@astryxdesign/core/Kbd';
import {Text} from '@astryxdesign/core/Text';
import {Heading} from '@astryxdesign/core/Heading';
import {AUTOMATIC_SCALE_LABEL, scaleOptionsWith} from './displayScale';
import type {RemoteScale} from './displaySettings';

export type ScaleMode = 'fit' | 'native';

/** How the remote desktop follows the window (RDP). */
export type RemoteDisplay = {
  scale: RemoteScale;
  resize: boolean;
  /** False when the server cannot change its display while connected. */
  canChange: boolean;
};

const CANNOT_CHANGE = 'This server cannot change its display while connected.';

// macOS has no 한/영 key and switches input sources with Caps Lock.
const toggleKey = /mac/i.test(navigator.platform) ? 'Caps Lock' : 'the 한/영 key or right Alt';

type Props = {
  isOpen: boolean;
  onOpenChange: (isOpen: boolean) => void;
  connectionName: string;
  clipboard: string;
  onClipboardChange: (text: string) => void;
  scaleMode: ScaleMode;
  onScaleModeChange: (mode: ScaleMode) => void;
  /** `null` for protocols whose remote display the menu cannot change. */
  remoteDisplay: RemoteDisplay | null;
  onRemoteScaleChange: (scale: RemoteScale) => void;
  onResizeToWindowChange: (resize: boolean) => void;
  showStats: boolean;
  onShowStatsChange: (show: boolean) => void;
  /** `null` when the connection has no remote input method to compose with. */
  remoteComposition: boolean | null;
  onRemoteCompositionChange: (isRemote: boolean) => void;
  onCtrlAltDel: () => void;
  onDisconnect: () => void;
};

/** The Ctrl+Alt+Shift menu: clipboard, display, input, stats and keys. */
export default function SessionMenu(props: Props) {
  const {isOpen, onOpenChange, connectionName, remoteDisplay} = props;
  const isDisplayFixed = remoteDisplay !== null && !remoteDisplay.canChange;
  const scaleValue =
    remoteDisplay === null || remoteDisplay.scale === 'auto' ? 'auto' : String(remoteDisplay.scale);
  return (
    <Drawer isOpen={isOpen} onOpenChange={onOpenChange} label="Session menu" width={360}>
      <Layout
        header={
          <DrawerHeader
            title={connectionName}
            subtitle={
              <HStack gap={1} vAlign="center">
                <Kbd keys="ctrl+alt+shift" />
                <Text color="secondary">to close</Text>
              </HStack>
            }
            onOpenChange={onOpenChange}
          />
        }
        content={
          <LayoutContent>
            <VStack gap={5}>
              <TextArea
                label="Clipboard"
                description="Text here is sent to the remote clipboard; copies on the remote side show up here."
                value={props.clipboard}
                onChange={props.onClipboardChange}
                rows={5}
                hasSpellCheck={false}
              />
              <VStack gap={4}>
                <Heading level={3}>Display</Heading>
                {remoteDisplay && (
                  <>
                    <Selector
                      label="Remote scale"
                      description="Text and apps on the remote desktop. Some apps only adjust after they restart or after you sign out."
                      value={scaleValue}
                      onChange={value => {
                        if (value) props.onRemoteScaleChange(value === 'auto' ? 'auto' : Number(value));
                      }}
                      options={[
                        {value: 'auto', label: AUTOMATIC_SCALE_LABEL},
                        ...scaleOptionsWith(scaleValue),
                      ]}
                      isDisabled={isDisplayFixed}
                      disabledMessage={CANNOT_CHANGE}
                    />
                    <Switch
                      label="Resize to window"
                      description="The remote desktop takes the size of this window."
                      value={remoteDisplay.resize}
                      onChange={props.onResizeToWindowChange}
                      isDisabled={isDisplayFixed}
                      disabledMessage={CANNOT_CHANGE}
                    />
                  </>
                )}
                <Selector
                  label="Fit"
                  description="How the picture is shown when its size differs from this window."
                  value={props.scaleMode}
                  onChange={value => props.onScaleModeChange((value ?? 'fit') as ScaleMode)}
                  options={[
                    {value: 'fit', label: 'Fit to window'},
                    {value: 'native', label: 'Native (1:1)'},
                  ]}
                />
              </VStack>
              {props.remoteComposition !== null && (
                <Switch
                  label="Compose Korean on the remote computer"
                  description={`Each key shows up as you press it, and the remote computer turns the keys into Hangul. Switch between Hangul and English with ${toggleKey}.`}
                  value={props.remoteComposition}
                  onChange={props.onRemoteCompositionChange}
                />
              )}
              <Switch
                label="Show performance stats"
                value={props.showStats}
                onChange={props.onShowStatsChange}
              />
            </VStack>
          </LayoutContent>
        }
        footer={
          <LayoutFooter>
            <HStack gap={2} hAlign="between">
              <Button label="Ctrl+Alt+Del" variant="secondary" onClick={props.onCtrlAltDel} />
              <Button label="Disconnect" variant="destructive" onClick={props.onDisconnect} />
            </HStack>
          </LayoutFooter>
        }
      />
    </Drawer>
  );
}
