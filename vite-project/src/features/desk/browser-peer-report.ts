import { SIGNALING_API_VERSION } from './constants';

declare global {
    const __LCXL_CLIENT_BUILD__: { readonly number: number; readonly commit: string };
}

/** Shared by browser desktop, file and terminal handshakes. */
export function appendBrowserPeerReport(url: URL) {
    const build = typeof __LCXL_CLIENT_BUILD__ === 'undefined' ? { number: 0, commit: '' } : __LCXL_CLIENT_BUILD__;
    url.searchParams.set('api_version', String(SIGNALING_API_VERSION));
    url.searchParams.set('build_number', String(build.number));
    url.searchParams.set('commit_hash', build.commit);
    url.searchParams.set('debug_build', String(import.meta.env.DEV));
    url.searchParams.set('operation_system', 'Web');
    url.searchParams.set('remote_desk_type', 'browser');
}
