import { describe, expect, it } from 'vitest';
import type { ConnectionModel } from '@/services/types';
import { assistantPaths, assistantConnections } from './assistant-paths';

const connection = (id: string, device?: string, client = 'stable-client') => ({ connection_id: id, device_id: device, version_info: { client_id: client } } as ConnectionModel);
describe('guided run device routing', () => {
    it('uses the Manager public handle and encodes the current connection route', () => {
        expect(assistantPaths([connection('live/connection', 'public-device')])).toEqual({ 'public-device': '/desk/live%2Fconnection/assistant' });
    });
    it('provides raw connection IDs without decoding assistant URLs and drops ambiguous devices', () => {
        expect(assistantConnections([connection('live/connection', 'device')])).toEqual({ device: 'live/connection' });
        expect(assistantConnections([connection('one'), connection('two'), connection('three')])).toEqual({});
        expect(assistantConnections([connection('', 'device')])).toEqual({});
    });
    it('keeps OSS identity stable across reconnects and refuses ambiguous connections', () => {
        expect(assistantPaths([connection('new-live')])).toEqual({ 'stable-client': '/desk/new-live/assistant' });
        expect(assistantPaths([connection('one'), connection('two'), connection('three')])).toEqual({});
    });
});
