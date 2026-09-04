import { describe, expect, it } from 'vitest';

import {
    ownerSelectableWindows,
    type ObservationEntry,
} from './use-device-assistant-observation';

describe('ownerSelectableWindows', () => {
    it('projects only complete edge-issued window references', () => {
        const entry: ObservationEntry = {
            phase: 'ready',
            requestId: 'request-1',
            outcome: {
                status: 'ok',
                data: {
                    ReadContext: {
                        DesktopUiInspect: {
                            owner_selectable_windows: [{
                                object_ref: {
                                    token: 'opaque-window',
                                    snapshot_id: 'worker-1:7',
                                    object_kind: 'window',
                                    expires_at: '2030-01-01T00:00:00Z',
                                },
                                title: 'Calculator',
                            }, {
                                object_ref: {
                                    token: 'not-a-window',
                                    snapshot_id: 'worker-1:7',
                                    object_kind: 'ui_element',
                                    expires_at: '2030-01-01T00:00:00Z',
                                },
                            }],
                        },
                    },
                },
            },
        };

        expect(ownerSelectableWindows(entry)).toEqual([{
            objectRef: {
                token: 'opaque-window',
                snapshot_id: 'worker-1:7',
                object_kind: 'window',
                expires_at: '2030-01-01T00:00:00Z',
            },
            title: 'Calculator',
        }]);
    });
});
