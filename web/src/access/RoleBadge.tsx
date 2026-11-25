import {Badge} from '@astryxdesign/core/Badge';
import {Text} from '@astryxdesign/core/Text';
import type {Role} from '../api';
import {roleLabel} from './roles';

/** A role as the Users and Groups tables show it. */
export default function RoleBadge({role}: {role: Role}) {
  if (role === 'user') return <Text>User</Text>;
  return <Badge variant={role === 'admin' ? 'info' : 'neutral'} label={roleLabel(role)} />;
}
