import type {Role} from '../api';

export const ROLES: {value: Role; label: string; description: string}[] = [
  {value: 'user', label: 'User', description: 'Opens the connections granted to them.'},
  {
    value: 'connection_manager',
    label: 'Connection manager',
    description: 'Opens every connection, manages connections and folders and who can open them.',
  },
  {value: 'admin', label: 'Administrator', description: 'Everything, including users, groups and roles.'},
];

export const roleLabel = (role: Role) => ROLES.find(r => r.value === role)?.label ?? role;
