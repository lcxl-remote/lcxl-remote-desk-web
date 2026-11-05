import { describe, expect, it } from 'vitest';
import { assistantAttentionPath } from './ai-assistant-attention-page';
import { deliveredCommentUrl } from './assistant-workbench-tasks';

describe('workbench links', () => {
    it('links only a confirmed delivery to the configured issue instance and original comment', () => {
        const delivered = { artifactId: 'reply-1', state: 'delivered', updatedAtMs: 1,
            commentId: 17, instance: 'https://forge.test/gitea', owner: 'Org', repository: 'Project', issueNumber: 4 };
        expect(deliveredCommentUrl(delivered)).toBe('https://forge.test/gitea/Org/Project/issues/4#issuecomment-17');
        expect(deliveredCommentUrl({ ...delivered, state: 'unknown' })).toBeUndefined();
        expect(deliveredCommentUrl({ ...delivered, owner: '..' })).toBeUndefined();
        expect(deliveredCommentUrl({ ...delivered, instance: 'javascript:alert(1)' })).toBeUndefined();
    });

    it('opens the original work and approval even when a device is offline', () => {
        const item = { attentionId: 'approval', sessionId: 'execution-session', clientConversationId: null,
            deviceId: '', reason: 'permission_approval' as const, requestId: 'permission-1', updatedAtUnixMs: 1,
            deadlineUnixMs: null, workbench: { workbenchId: 'root', requestId: 'request-a', workerTaskId: 'worker-a' } };
        expect(assistantAttentionPath(item, false))
            .toBe('/ai-assistant/workbench?workbench=root&task=request-a&worker=worker-a&permission=permission-1');
        expect(assistantAttentionPath({ ...item, reason: 'worker_question', requestId: 'question-1' }, false))
            .toBe('/ai-assistant/workbench?workbench=root&task=request-a&worker=worker-a');
        expect(assistantAttentionPath({ ...item, workbench: null, deviceId: 'offline' }, false)).toBeUndefined();
    });
});
