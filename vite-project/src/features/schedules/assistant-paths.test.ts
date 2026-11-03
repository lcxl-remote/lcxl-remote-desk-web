import { describe, expect, it } from 'vitest';
import type { ConnectionModel } from '@/services/types';
import { assistantPaths, assistantConnections } from './assistant-paths';

const connection = (id: string, device = 'stable-client') => ({ connection_id: id, device_id: device, version_info: null, peer: { remote_desk_type:'server', operation_system:'Other', display_name:null }, exec_capabilities: { available_exec_shells: [], exec_pty:false, exec_pty_elevation:false, max_ai_command_runtime_ms:null }, ai_assistant_settings: null } as ConnectionModel);
describe('guided run device routing', () => {
    it('uses the Manager public handle and uses it for stable navigation', () => {
        expect(assistantPaths([connection('live/connection', 'public-device')])).toEqual({ 'public-device': '/desk/device/public-device/assistant' });
    });
    it('provides raw connection IDs without decoding assistant URLs and drops ambiguous devices', () => {
        expect(assistantConnections([connection('live/connection', 'device')])).toEqual({ device: 'live/connection' });
        expect(assistantConnections([connection('one'), connection('two'), connection('three')])).toEqual({});
        expect(assistantConnections([connection('', 'device')])).toEqual({});
    });
    it('keeps OSS identity stable across reconnects and refuses ambiguous connections', () => {
        expect(assistantPaths([connection('new-live')])).toEqual({ 'stable-client': '/desk/device/stable-client/assistant' });
        expect(assistantPaths([connection('one'), connection('two'), connection('three')])).toEqual({});
    });
});
