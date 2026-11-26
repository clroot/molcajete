import type {ReactNode} from 'react';
import {Drawer, DrawerHeader} from '@astryxdesign/core/Drawer';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {TextArea} from '@astryxdesign/core/TextArea';
import {Selector} from '@astryxdesign/core/Selector';
import {Switch} from '@astryxdesign/core/Switch';
import {Slider} from '@astryxdesign/core/Slider';
import {Button} from '@astryxdesign/core/Button';
import {Kbd} from '@astryxdesign/core/Kbd';
import {Text} from '@astryxdesign/core/Text';
import {Heading} from '@astryxdesign/core/Heading';
import {AUTOMATIC_SCALE_LABEL, scaleOptionsWith} from './displayScale';
import {TEXT_SIZE, clampTextSize, type RemoteScale} from './displaySettings';
import {terminalKeys} from './terminal';
import type {Restrictions} from './useConnection';
import {FolderIcon} from '@heroicons/react/24/outline';

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
  /** Terminals copy and paste with shortcuts, shown under the clipboard. */
  isTerminal: boolean;
  /** False on a device used by finger: shortcuts are not shown there. */
  hasKeyboard: boolean;
  /** What the connection turns off. */
  restrictions: Restrictions;
  clipboard: string;
  onClipboardChange: (text: string) => void;
  scaleMode: ScaleMode;
  onScaleModeChange: (mode: ScaleMode) => void;
  /** `null` for protocols whose remote display the menu cannot change. */
  remoteDisplay: RemoteDisplay | null;
  onRemoteScaleChange: (scale: RemoteScale) => void;
  onResizeToWindowChange: (resize: boolean) => void;
  /** The text size of a terminal, in points; `null` for a desktop. */
  textSize: number | null;
  onTextSizeChange: (size: number) => void;
  /** Whether the terminal key bar shows; `null` when the session has none to show. */
  keyBar: boolean | null;
  onKeyBarChange: (shows: boolean) => void;
  /** Whether the whole screen goes as video; `null` when the session streams no video. */
  fullScreenVideo: boolean | null;
  onFullScreenVideoChange: (isFullScreen: boolean) => void;
  showStats: boolean;
  onShowStatsChange: (show: boolean) => void;
  /** `null` when the connection has no remote input method to compose with. */
  remoteComposition: boolean | null;
  onRemoteCompositionChange: (isRemote: boolean) => void;
  onCtrlAltDel: () => void;
  onDisconnect: () => void;
  /** Starting and stopping a recording, when the gateway knows the session. */
  recording?: ReactNode;
  /** Who joined and the share links, when the gateway knows the session. */
  sharing?: ReactNode;
  /** Opens the file panel; `null` when the connection shares no files. */
  onOpenFiles: (() => void) | null;
};

const CLIPBOARD_HELP = {
  both: 'Text here is sent to the remote clipboard; copies on the remote side show up here.',
  copyOnly: 'Copies on the remote side show up here. Pasting into the remote side is turned off.',
  pasteOnly: 'Text here is sent to the remote clipboard. Copying from the remote side is turned off.',
};

/**
 * The clipboard text and, in terminals, the copy and paste shortcuts, as far
 * as the connection allows them.
 */
function Clipboard({restrictions, isTerminal, hasKeyboard, keyBar, clipboard, onClipboardChange}: Props) {
  const canCopy = !restrictions.disableCopy;
  // A read-only connection takes nothing from the browser, the clipboard
  // included.
  const canPaste = !restrictions.disablePaste && !restrictions.readOnly;
  if (!canCopy && !canPaste) {
    return <Text color="secondary">Copying and pasting are turned off for this connection.</Text>;
  }
  return (
    <>
      <TextArea
        label="Clipboard"
        description={
          canCopy && canPaste
            ? CLIPBOARD_HELP.both
            : canCopy
              ? CLIPBOARD_HELP.copyOnly
              : CLIPBOARD_HELP.pasteOnly
        }
        value={clipboard}
        onChange={onClipboardChange}
        isReadOnly={!canPaste}
        rows={5}
        hasSpellCheck={false}
      />
      {isTerminal && !hasKeyboard && keyBar && canPaste && (
        <Text type="supporting" color="secondary">
          Paste, in the bar under the terminal, pastes from this device, or this text where the browser does
          not share its clipboard.
        </Text>
      )}
      {isTerminal && hasKeyboard && (canCopy || canPaste) && (
        <VStack gap={2}>
          {canCopy && (
            <HStack gap={2} vAlign="center">
              <Kbd keys={terminalKeys.copy} />
              <Text color="secondary">copies the selected text</Text>
            </HStack>
          )}
          {canPaste && (
            <HStack gap={2} vAlign="center">
              <Kbd keys={terminalKeys.paste} />
              <Text color="secondary">pastes from this computer</Text>
            </HStack>
          )}
        </VStack>
      )}
    </>
  );
}

type FullScreenVideoSwitchProps = {value: boolean; onChange: (isFullScreen: boolean) => void};

/** Streams the whole screen as video instead of only the parts that move a lot. */
export function FullScreenVideoSwitch({value, onChange}: FullScreenVideoSwitchProps) {
  return (
    <Switch
      label="Stream the whole screen as video"
      description="Smoother for games, 3D and full-screen video. Text looks blurry while the screen moves and sharp again once it stops."
      value={value}
      onChange={onChange}
    />
  );
}

/** The Ctrl+Alt+Shift menu: files, clipboard, display, input, stats, recording, sharing and keys. */
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
              props.hasKeyboard ? (
                <HStack gap={1} vAlign="center">
                  <Kbd keys="ctrl+alt+shift" />
                  <Text color="secondary">to close</Text>
                </HStack>
              ) : undefined
            }
            onOpenChange={onOpenChange}
          />
        }
        content={
          <LayoutContent>
            <VStack gap={5}>
              {props.onOpenFiles && (
                <Button
                  label="Files"
                  icon={<FolderIcon width={16} height={16} />}
                  onClick={props.onOpenFiles}
                />
              )}
              <Clipboard {...props} />
              {props.textSize !== null && (
                <VStack gap={4}>
                  <Heading level={3}>Terminal</Heading>
                  <Slider
                    label="Text size"
                    description="Kept for this connection in this browser. On a touch screen, pinching the terminal changes it too."
                    value={clampTextSize(props.textSize)}
                    onChange={(size: number | [number, number]) => props.onTextSizeChange(size as number)}
                    min={TEXT_SIZE.min}
                    max={TEXT_SIZE.max}
                    step={1}
                    valueDisplay="text"
                    formatValue={size => `${size} pt`}
                    marks={[6, 9, 12, 15, 18].map(value => ({value}))}
                  />
                  {props.keyBar !== null && (
                    <Switch
                      label="Show terminal keys"
                      description="Esc, Tab, Ctrl, Alt and the arrow keys in a bar under the terminal, for typing on an on-screen keyboard."
                      value={props.keyBar}
                      onChange={props.onKeyBarChange}
                    />
                  )}
                </VStack>
              )}
              {/* A terminal follows the window, so there is nothing to fit. */}
              {!props.isTerminal && (
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
                  {props.fullScreenVideo !== null && (
                    <FullScreenVideoSwitch
                      value={props.fullScreenVideo}
                      onChange={props.onFullScreenVideoChange}
                    />
                  )}
                </VStack>
              )}
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
              {props.recording}
              {props.sharing}
            </VStack>
          </LayoutContent>
        }
        footer={
          <LayoutFooter>
            <HStack gap={2} hAlign="between">
              <Button
                label="Ctrl+Alt+Del"
                variant="secondary"
                onClick={props.onCtrlAltDel}
                isDisabled={props.restrictions.readOnly}
              />
              <Button label="Disconnect" variant="destructive" onClick={props.onDisconnect} />
            </HStack>
          </LayoutFooter>
        }
      />
    </Drawer>
  );
}
