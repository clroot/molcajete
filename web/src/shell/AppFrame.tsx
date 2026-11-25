// Built from the Astryx `shell-side-nav` page template.
import {useState, type ReactNode} from 'react';
import {AppShell} from '@astryxdesign/core/AppShell';
import {SideNav, SideNavHeading, SideNavItem, SideNavSection} from '@astryxdesign/core/SideNav';
import {
  ComputerDesktopIcon,
  UsersIcon,
  UserGroupIcon,
  KeyIcon,
  ArrowRightStartOnRectangleIcon,
  UserCircleIcon,
} from '@heroicons/react/24/outline';
import type {User} from '../api';
import {href, type Route} from '../route';
import Logo from './Logo';
import ChangePasswordDialog from '../auth/ChangePasswordDialog';

type Props = {
  user: User;
  route: Route;
  onSignOut: () => void;
  children: ReactNode;
};

export default function AppFrame({user, route, onSignOut, children}: Props) {
  const [isPasswordOpen, setIsPasswordOpen] = useState(false);

  return (
    <AppShell
      contentPadding={0}
      sideNav={
        <SideNav
          collapsible
          resizable={{defaultWidth: 260, minWidth: 220, maxWidth: 360, autoSaveId: 'molcajete-sidenav'}}
          header={
            <SideNavHeading heading="molcajete" icon={<Logo size={24} />} headingHref={href.connections} />
          }
          footer={
            <SideNavSection title="Account" isHeaderHidden>
              <SideNavItem label={user.username} icon={UserCircleIcon} isDisabled />
              <SideNavItem label="Change password" icon={KeyIcon} onClick={() => setIsPasswordOpen(true)} />
              <SideNavItem label="Sign out" icon={ArrowRightStartOnRectangleIcon} onClick={onSignOut} />
            </SideNavSection>
          }>
          <SideNavSection title="Menu" isHeaderHidden>
            <SideNavItem
              label="Connections"
              icon={ComputerDesktopIcon}
              href={href.connections}
              isSelected={route.name === 'connections'}
            />
            {user.isAdmin && (
              <>
                <SideNavItem
                  label="Users"
                  icon={UsersIcon}
                  href={href.users}
                  isSelected={route.name === 'users'}
                />
                <SideNavItem
                  label="Groups"
                  icon={UserGroupIcon}
                  href={href.groups}
                  isSelected={route.name === 'groups'}
                />
              </>
            )}
          </SideNavSection>
        </SideNav>
      }>
      {children}
      <ChangePasswordDialog isOpen={isPasswordOpen} onOpenChange={setIsPasswordOpen} onChanged={onSignOut} />
    </AppShell>
  );
}
