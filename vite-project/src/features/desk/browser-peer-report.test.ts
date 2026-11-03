import { expect, it } from 'vitest';
import { appendBrowserPeerReport } from './browser-peer-report';

it('reports this browser build consistently without adding a node credential', () => {
    const url = new URL('https://fixture.invalid/api/desk/signaling?purpose=file');
    appendBrowserPeerReport(url);
    expect(url.searchParams.get('api_version')).toBe('1');
    expect(url.searchParams.get('build_number')).toBe(String(__LCXL_CLIENT_BUILD__.number));
    expect(url.searchParams.get('commit_hash')).toBe(__LCXL_CLIENT_BUILD__.commit);
    expect(url.searchParams.get('operation_system')).toBe('Web');
    expect(url.searchParams.get('remote_desk_type')).toBe('browser');
    expect(url.searchParams.has('token')).toBe(false);
    expect(url.searchParams.get('purpose')).toBe('file');
});
