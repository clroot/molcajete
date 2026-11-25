import {Drawer, DrawerHeader} from '@astryxdesign/core/Drawer';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {TextArea} from '@astryxdesign/core/TextArea';
import {Selector} from '@astryxdesign/core/Selector';
import {Switch} from '@astryxdesign/core/Switch';
import {Button} from '@astryxdesign/core/Button';
import {Kbd} from '@astryxdesign/core/Kbd';
import {Text} from '@astryxdesign/core/Text';

export type ScaleMode = 'fit' | 'native';

type Props = {
  isOpen: boolean;
  onOpenChange: (isOpen: boolean) => void;
  connectionName: string;
  clipboard: string;
  onClipboardChange: (text: string) => void;
  scaleMode: ScaleMode;
  onScaleModeChange: (mode: ScaleMode) => void;
  showStats: boolean;
  onShowStatsChange: (show: boolean) => void;
  onCtrlAltDel: () => void;
  onDisconnect: () => void;
};

/** The Ctrl+Alt+Shift menu: clipboard, display scale, stats and keys. */
export default function SessionMenu(props: Props) {
  const {isOpen, onOpenChange, connectionName} = props;
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
              <Selector
                label="Display scale"
                value={props.scaleMode}
                onChange={value => props.onScaleModeChange((value ?? 'fit') as ScaleMode)}
                options={[
                  {value: 'fit', label: 'Fit to window'},
                  {value: 'native', label: 'Native (1:1)'},
                ]}
              />
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
