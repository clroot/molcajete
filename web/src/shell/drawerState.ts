/** Which record a create/edit drawer is showing, if any. */
export type DrawerState = {mode: 'closed'} | {mode: 'create'} | {mode: 'edit'; id: string};

export const CLOSED: DrawerState = {mode: 'closed'};
export const CREATE: DrawerState = {mode: 'create'};
export const edit = (id: string): DrawerState => ({mode: 'edit', id});
