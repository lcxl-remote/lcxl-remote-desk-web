/**
 * @see https://umijs.org/docs/max/access#access
 * */
export default function access(initialState: { currentUser?: API.CurrentUser; startupMode?: string } | undefined) {
  const { currentUser, startupMode } = initialState ?? {};
  const mode = startupMode || 'default';
  return {
    canAdmin: currentUser && currentUser.access === 'admin',
    canDesk: mode === 'default' || mode === 'desk-server',
    canSignaling: mode === 'default' || mode === 'signaling',
  };
}
