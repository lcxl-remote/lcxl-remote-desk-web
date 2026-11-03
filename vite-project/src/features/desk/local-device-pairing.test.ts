import { describe, expect, it } from 'vitest';
import { canPairLocalDevice } from './local-device-pairing';

const current = { isSuccess: true, isFetchedAfterMount: true, isFetching: false, isError: false,
    data: { success: true, data: { local_device_id: 'local' } } };
describe('local pairing target', () => {
    it('requires an exact match with the local host identity', () => {
        expect(canPairLocalDevice('local', current)).toBe(true);
        for (const target of ['remote', '', null, undefined]) expect(canPairLocalDevice(target, current)).toBe(false);
        expect(canPairLocalDevice('local', { ...current, data: { success: true, data: { local_device_id: null } } })).toBe(false);
    });
    it('fails closed for unavailable info, including pure signal 404 and stale success data', () => {
        expect(canPairLocalDevice('local', { ...current, isSuccess: false, isError: true, data: undefined })).toBe(false);
        expect(canPairLocalDevice('local', { ...current, isFetchedAfterMount: false })).toBe(false);
        expect(canPairLocalDevice('local', { ...current, isFetching: true })).toBe(false);
        expect(canPairLocalDevice('local', { ...current, isError: true })).toBe(false);
        expect(canPairLocalDevice('local', { ...current, data: { ...current.data, success: false } })).toBe(false);
    });
});
