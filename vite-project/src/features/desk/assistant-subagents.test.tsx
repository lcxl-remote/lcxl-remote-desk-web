import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { useState, type ReactNode } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { AssistantSubagentApprovalNotice, AssistantSubagents, AssistantConversationTabs } from './assistant-subagents';
import type { useAiAssistantSubagents } from './use-ai-assistant-subagents';
import type { PersistedSnapshot, AiAssistantMessage } from './use-ai-assistant-chat';
import type { AiAssistantSubAgentSummary, PermissionRequestDto } from '@/services/types';

const lifecycle = vi.hoisted(() => ({ attachments: [] as string[], images: [] as string[] }));
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
vi.mock('./assistant-attachments', async () => {
    const { useEffect } = await import('react');
    return {
        AssistantAttachments: ({ sessionId }: { sessionId: string }) => {
            useEffect(() => () => { lifecycle.attachments.push(sessionId); }, [sessionId]);
            return <div data-testid="child-materials" data-session={sessionId} />;
        },
        AssistantResultAttachments: ({ sessionId }: { sessionId: string }) =>
            <div data-testid="child-result-materials" data-session={sessionId} />,
    };
});
vi.mock('./assistant-images', async () => {
    const { useEffect } = await import('react');
    return { AssistantImages: ({ sessionId, messages, renderMessage }: { sessionId: string;
        messages: AiAssistantMessage[]; renderMessage: (message: AiAssistantMessage) => ReactNode }) => {
        useEffect(() => () => { lifecycle.images.push(sessionId); }, [sessionId]);
        return <div data-testid="child-images" data-session={sessionId}>{messages.map(renderMessage)}</div>;
    } };
});

function agentsFor(childId: string): ReturnType<typeof useAiAssistantSubagents> {
    const task: AiAssistantSubAgentSummary = { task_id: `task-${childId}`, child_session_id: childId,
        group_id: 'original-group', name: `Research ${childId}`, state: 'running', input_revision: 1,
        control_revision: 1, state_revision: 2, source_goal_id: null,
        source: { kind: 'user_input', input_revision: 1 }, wait_reason: null,
        created_at: '2026-09-30T00:00:00Z', updated_at: '2026-09-30T00:00:01Z' };
    const session: PersistedSnapshot = { sessionId: childId, seq: 1, active: false, inputRevision: 1,
        controlRevision: 1, mainStopped: false, messages: [],
        contextAttachments: [{ id: 'context', kind: 'interactive_session', capabilityId: 'desktop.ui.inspect',
            providerId: 'desktop.ui', state: 'active', expiresAtUnixMs: 100, createdAtUnixMs: 1,
            displaySummary: `Context ${childId}` }],
        subagents: { parent_session_id: 'root', task, active_tasks: [], tasks: null, attention_tasks: [], attention_count: 0 } };
    return {
        tasks: [task], total: 1, attentionCount: 0, unfinished: 1, hasMore: false, loading: false,
        loadMore: vi.fn(), selected: task, detail: { result: { task, objective: 'Read original evidence',
            acceptance_criteria: ['Cite the source'], report: null, failure_reason: null }, session },
        detailLoading: false, openDetail: vi.fn(), closeDetail: vi.fn(), updating: false, error: null,
        control: vi.fn(), decidePermission: vi.fn(), updateDirectory: vi.fn(), revokeGrant: vi.fn(),
        cancelProviderTask: vi.fn(), cancelCommandTask: vi.fn(), historyLoading: false, hasMoreMessages: false,
        loadOlderMessages: vi.fn(), childTools: [], childMessages: [{ id: `receipt-${childId}`,
            role: 'tool_result', text: `Original output ${childId}` }],
    };
}

afterEach(() => { cleanup(); lifecycle.attachments.length = 0; lifecycle.images.length = 0; vi.clearAllMocks(); vi.restoreAllMocks(); });
it('keeps an oversized UTF-8 adjustment draft without submitting an invalid child control request', () => {
    const agents = agentsFor('child-1');
    render(<AssistantSubagents agents={agents} connected canDecide mainStopped={false} />);
    const input = screen.getByRole('textbox', { name: 'New requirements for this subtask' });
    const message = '中'.repeat(6_000);
    fireEvent.change(input, { target: { value: message } });
    const submit = screen.getByRole('button', { name: 'Apply adjustment' });
    expect(submit).toBeDisabled();
    fireEvent.click(submit);
    expect(input).toHaveValue(message);
    expect(agents.control).not.toHaveBeenCalled();
});
describe('subagent approval notice', () => {
    it('shows pending approvals outside the collapsed task list and opens the original child', () => {
        const agents = agentsFor('child-1');
        agents.tasks[0] = { ...agents.tasks[0], state: 'waiting_approval', wait_reason: 'owner_approval' };
        agents.selected = null; agents.detail = null;
        render(<><AssistantSubagentApprovalNotice agents={agents} />
            <AssistantSubagents agents={agents} connected canDecide mainStopped={false} /></>);
        expect(screen.getByRole('status')).toHaveTextContent('1 subtasks awaiting approval');
        const entry = screen.getByRole('button', { name: 'View approval · Research child-1' });
        expect(entry).toBeVisible();
        fireEvent.click(entry);
        expect(agents.openDetail).toHaveBeenCalledExactlyOnceWith(agents.tasks[0]);
        expect(agents.decidePermission).not.toHaveBeenCalled();
    });

    it('counts waiting tasks independently of unread updates and removes resolved approvals', () => {
        const agents = agentsFor('child-1');
        agents.tasks = ['child-1', 'child-2'].map(id => ({ ...agentsFor(id).tasks[0], state: 'waiting_approval' }));
        const view = render(<AssistantSubagentApprovalNotice agents={agents} />);
        expect(screen.getByRole('status')).toHaveTextContent('2 subtasks awaiting approval');
        agents.tasks[0] = { ...agents.tasks[0], state: 'running' };
        view.rerender(<AssistantSubagentApprovalNotice agents={agents} />);
        expect(screen.getByRole('status')).toHaveTextContent('1 subtasks awaiting approval');
        expect(screen.queryByRole('button', { name: /Research child-1/ })).toBeNull();
        agents.tasks[1] = { ...agents.tasks[1], state: 'completed' };
        agents.attentionCount = 2;
        view.rerender(<AssistantSubagentApprovalNotice agents={agents} />);
        expect(screen.queryByRole('status')).toBeNull();
    });
});

describe('subagent material ownership', () => {
    it('binds process images, text references and stored materials to the original child after main stop', () => {
        render(<AssistantSubagents agents={agentsFor('child-1')} connected canDecide mainStopped />);
        for (const name of ['child-materials', 'child-result-materials', 'child-images'])
            expect(screen.getByTestId(name)).toHaveAttribute('data-session', 'child-1');
        expect(screen.queryByText('Context child-1')).toBeNull();
        expect(screen.queryByText('Read original evidence')).toBeNull();
        expect(screen.queryByRole('textbox', { name: 'Working directory path' })).toBeNull();
        expect(screen.getByText('Original output child-1')).toBeInTheDocument();
        expect(lifecycle.attachments).toEqual([]);
        expect(lifecycle.images).toEqual([]);
    });

    it('unmounts prior material viewers when switching child details within the same root', () => {
        const view = render(<AssistantSubagents agents={agentsFor('child-1')} connected canDecide mainStopped={false} />);
        view.rerender(<AssistantSubagents agents={agentsFor('child-2')} connected canDecide mainStopped={false} />);
        for (const name of ['child-materials', 'child-result-materials', 'child-images'])
            expect(screen.getByTestId(name)).toHaveAttribute('data-session', 'child-2');
        expect(screen.queryByText('Context child-1')).toBeNull();
        expect(screen.queryByText('Original output child-1')).toBeNull();
        expect(lifecycle.attachments).toEqual(['child-1']);
        expect(lifecycle.images).toEqual(['child-1']);
    });
});


describe('conversation tabs', () => {
    it('preserves separate adjustment drafts across tabs and keeps a rejected adjustment', async () => {
        const control = vi.fn().mockResolvedValue(false);
        function Workspace() {
            const [id, setId] = useState('child-1');
            const [drafts, setDrafts] = useState<Record<string, string>>({});
            const agents = agentsFor(id); agents.control = control;
            return <><button onClick={() => setId(id === 'child-1' ? 'child-2' : 'child-1')}>Switch</button>
                <AssistantSubagents key={id} agents={agents} connected canDecide mainStopped={false}
                    adjustment={drafts[id] ?? ''} onAdjustmentChange={text => setDrafts(previous => ({ ...previous, [id]: text }))} /></>;
        }
        render(<Workspace />);
        const input = () => screen.getByRole('textbox', { name: 'New requirements for this subtask' });
        fireEvent.change(input(), { target: { value: 'First draft' } });
        fireEvent.click(screen.getByRole('button', { name: 'Switch' }));
        expect(input()).toHaveValue('');
        fireEvent.change(input(), { target: { value: 'Second draft' } });
        fireEvent.click(screen.getByRole('button', { name: 'Switch' }));
        expect(input()).toHaveValue('First draft');
        fireEvent.click(screen.getByRole('button', { name: 'Apply adjustment' }));
        await waitFor(() => expect(control).toHaveBeenCalledOnce());
        expect(input()).toHaveValue('First draft');
    });
    it('shows an approving main tab even before any child has been created', () => {
        const agents = agentsFor('child-1'); agents.tasks = []; agents.selected = null; agents.detail = null;
        render(<AssistantConversationTabs agents={agents} mainNeedsApproval />);
        expect(screen.getByRole('tab', { name: /Main conversation/ })).toBeVisible();
        expect(screen.getByText('Main conversation')).toHaveClass('assistant-approval-tab-title');
    });
    it('uses the matching child snapshot for revalidation attention and ignores terminal tasks', () => {
        const agents = agentsFor('child-1');
        agents.detail!.session.permissionRequests = [{ state: 'needs_revalidation' } as PermissionRequestDto];
        const view = render(<AssistantConversationTabs agents={agents} />);
        expect(screen.getByText('Research child-1')).toHaveClass('assistant-approval-tab-title');
        agents.detail!.result.task = { ...agents.detail!.result.task, task_id: 'another-task' };
        view.rerender(<AssistantConversationTabs agents={agents} />);
        expect(screen.getByText('Research child-1')).not.toHaveClass('assistant-approval-tab-title');
        agents.tasks[0] = { ...agents.tasks[0], state: 'completed' };
        agents.detail!.result.task = agents.tasks[0];
        view.rerender(<AssistantConversationTabs agents={agents} />);
        expect(screen.getByText('Research child-1')).not.toHaveClass('assistant-approval-tab-title');
    });
    it('blinks main and child titles until approval resolves, independently of selection and unread status', () => {
        const agents = agentsFor('child-1');
        agents.tasks[0] = { ...agents.tasks[0], state: 'waiting_approval' };
        const view = render(<AssistantConversationTabs agents={agents} mainNeedsApproval />);
        expect(screen.getByText('Main conversation')).toHaveAttribute('data-approval-pending', 'true');
        expect(screen.getByText('Research child-1')).toHaveClass('assistant-approval-tab-title');
        agents.selected = null;
        view.rerender(<AssistantConversationTabs agents={agents} mainNeedsApproval attentionTasks={[]} />);
        expect(screen.getByText('Research child-1')).toHaveClass('assistant-approval-tab-title');
        agents.tasks[0] = { ...agents.tasks[0], state: 'completed' };
        view.rerender(<AssistantConversationTabs agents={agents} />);
        expect(screen.getByText('Main conversation')).not.toHaveAttribute('data-approval-pending');
        expect(screen.getByText('Research child-1')).not.toHaveClass('assistant-approval-tab-title');
    });

    it('opens shared child details and context on demand without putting them in the transcript', () => {
        const agents = agentsFor('child-1');
        render(<AssistantSubagents agents={agents} connected canDecide mainStopped={false} />);
        expect(screen.queryByText('Read original evidence')).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'Task details' }));
        expect(screen.getByRole('dialog')).toHaveTextContent('Read original evidence');
        expect(screen.getByTestId('assistant-shared-transcript')).not.toHaveTextContent('Read original evidence');
        fireEvent.click(screen.getByRole('button', { name: 'Current context' }));
        expect(screen.getByRole('dialog')).toHaveTextContent('Context child-1');
    });
    it('switches to the original child and returns to main without a task control action', () => {
        const agents = agentsFor('child-1');
        render(<AssistantConversationTabs agents={agents} attentionTasks={agents.tasks} />);
        expect(screen.getByRole('tab', { name: 'Main conversation' })).toHaveAttribute('aria-selected', 'false');
        expect(screen.getByLabelText('Unread update')).toBeInTheDocument();
        fireEvent.click(screen.getByRole('tab', { name: 'Main conversation' }));
        expect(agents.closeDetail).toHaveBeenCalledOnce();
        expect(agents.control).not.toHaveBeenCalled();
    });

    it('moves ended tasks behind unfinished tasks and supports keyboard selection', () => {
        const agents = agentsFor('child-1');
        const first = { ...agents.tasks[0], created_at: '2026-10-05T01:00:00Z', state: 'completed' as const };
        const second = { ...agentsFor('child-2').tasks[0], created_at: '2026-10-05T02:00:00Z' };
        agents.tasks = [second, first]; agents.selected = null;
        const view = render(<AssistantConversationTabs agents={agents} />);
        expect(screen.getAllByRole('tab').map(tab => tab.textContent)).toEqual([
            'Main conversation', expect.stringContaining('Research child-2'), expect.stringContaining('Research child-1')]);
        const main = screen.getByRole('tab', { name: 'Main conversation' }); main.focus();
        fireEvent.keyDown(main, { key: 'ArrowRight' });
        expect(agents.openDetail).toHaveBeenCalledWith(second);
        agents.tasks = [{ ...second, state: 'completed' }, first];
        view.rerender(<AssistantConversationTabs agents={agents} />);
        expect(screen.getAllByRole('tab')[1]).toHaveTextContent('Research child-1');
    });

    it('puts approvals first and all terminal states last without changing selection or unread state', () => {
        const agents = agentsFor('child-1');
        const task = (id: string, state: AiAssistantSubAgentSummary['state'], date: string) => ({ ...agentsFor(id).tasks[0], state, created_at: date });
        const running = task('running', 'running', '02');
        const pending = task('pending', 'waiting_approval', '05');
        agents.tasks = [task('failed', 'failed', '04'), task('cancelled', 'cancelled', '03'),
            pending, task('finished', 'completed', '01'), task('queued', 'queued', '06'), running];
        agents.selected = running; agents.detail = null;
        const view = render(<AssistantConversationTabs agents={agents} attentionTasks={[{ task_id: 'task-finished' }]} />);
        const order = () => screen.getAllByRole('tab').map(tab => tab.id);
        expect(order()).toEqual(['assistant-tab-main', 'assistant-tab-task-pending', 'assistant-tab-task-running',
            'assistant-tab-task-queued', 'assistant-tab-task-finished', 'assistant-tab-task-cancelled', 'assistant-tab-task-failed']);
        expect(screen.getByRole('tab', { name: /Research running/ })).toHaveAttribute('aria-selected', 'true');
        agents.tasks = agents.tasks.map(task => task.task_id === pending.task_id ? { ...task, state: 'running' } : task);
        view.rerender(<AssistantConversationTabs agents={agents} />);
        expect(order().slice(1, 4)).toEqual(['assistant-tab-task-running', 'assistant-tab-task-pending', 'assistant-tab-task-queued']);
        expect(screen.getByRole('tab', { name: /Research running/ })).toHaveAttribute('aria-selected', 'true');
        expect(agents.openDetail).not.toHaveBeenCalled();
        expect(agents.control).not.toHaveBeenCalled();
    });

    it('promotes a running child with matching pending revalidation above another unfinished child', () => {
        const agents = agentsFor('child-1');
        agents.tasks = [{ ...agentsFor('child-2').tasks[0], created_at: '01' }, { ...agents.tasks[0], created_at: '02' }];
        agents.detail!.session.permissionRequests = [{ state: 'needs_revalidation' } as PermissionRequestDto];
        render(<AssistantConversationTabs agents={agents} />);
        expect(screen.getAllByRole('tab')[1]).toHaveTextContent('Research child-1');
    });

    it('keeps pagination outside the scroll viewport and reaches the last child through the switch menu', async () => {
        const agents = agentsFor('child-1');
        agents.tasks = Array.from({ length: 32 }, (_, index) => agentsFor(`child-${index.toString().padStart(2, '0')}`).tasks[0]);
        agents.selected = null; agents.detail = null; agents.hasMore = true;
        render(<AssistantConversationTabs agents={agents} />);
        const viewport = screen.getByTestId('assistant-tab-viewport');
        const more = screen.getByRole('button', { name: 'Load earlier subtasks' });
        expect(viewport).not.toContainElement(more);
        expect(viewport).not.toContainElement(screen.getByRole('tab', { name: 'Main conversation' }));
        fireEvent.click(more); expect(agents.loadMore).toHaveBeenCalledOnce();
        fireEvent.pointerDown(screen.getByRole('button', { name: 'Switch conversation' }), { button: 0, ctrlKey: false, pointerType: 'mouse' });
        fireEvent.click(await screen.findByRole('menuitem', { name: /Research child-31/ }));
        expect(agents.openDetail).toHaveBeenCalledExactlyOnceWith(agents.tasks[31]);
        expect(agents.control).not.toHaveBeenCalled();
    });

    it('scrolls overflowing tabs with buttons, mouse wheel and keyboard, then reveals selection after reorder', () => {
        const agents = agentsFor('child-1');
        agents.tasks = [agents.tasks[0], agentsFor('child-2').tasks[0]];
        agents.selected = null; agents.detail = null;
        const view = render(<AssistantConversationTabs agents={agents} />);
        const viewport = screen.getByTestId('assistant-tab-viewport');
        Object.defineProperties(viewport, { clientWidth: { configurable: true, value: 100 }, scrollWidth: { configurable: true, value: 800 } });
        const scrollBy = vi.fn(); Object.defineProperty(viewport, 'scrollBy', { configurable: true, value: scrollBy });
        const reveal = vi.fn(); screen.getByRole('tab', { name: /Research child-2/ }).scrollIntoView = reveal;
        fireEvent.scroll(viewport);
        fireEvent.click(screen.getByRole('button', { name: 'Scroll conversation tabs right' }));
        expect(scrollBy).toHaveBeenCalledWith({ left: 80, behavior: 'smooth' });
        fireEvent.wheel(viewport, { deltaY: 200, deltaX: 0 }); expect(viewport.scrollLeft).toBe(200);
        const main = screen.getByRole('tab', { name: 'Main conversation' }); main.focus();
        fireEvent.keyDown(main, { key: 'End' });
        expect(agents.openDetail).toHaveBeenCalledWith(agents.tasks[1]);
        expect(reveal).toHaveBeenCalled();
        reveal.mockClear(); agents.selected = agents.tasks[1];
        agents.tasks[1] = { ...agents.tasks[1], state: 'waiting_approval' };
        view.rerender(<AssistantConversationTabs agents={agents} />);
        expect(reveal).toHaveBeenCalledWith({ block: 'nearest', inline: 'nearest' });
    });

    it('reveals the start of an oversized selected tab so its title stays visible in a narrow viewport', () => {
        const agents = agentsFor('child-1'); agents.selected = null;
        const view = render(<AssistantConversationTabs agents={agents} />);
        const viewport = screen.getByTestId('assistant-tab-viewport');
        const tab = screen.getByRole('tab', { name: /Research child-1/ });
        Object.defineProperty(viewport, 'clientWidth', { configurable: true, value: 100 });
        Object.defineProperty(tab, 'offsetWidth', { configurable: true, value: 256 });
        const reveal = vi.fn(); tab.scrollIntoView = reveal;
        agents.selected = agents.tasks[0]; view.rerender(<AssistantConversationTabs agents={agents} />);
        expect(reveal).toHaveBeenCalledWith({ block: 'nearest', inline: 'start' });
    });

    it('renders child conversation directly with shared reasoning and no details dialog', () => {
        const agents = agentsFor('child-1');
        agents.childMessages = [{ id: 'thinking', role: 'assistant', text: 'Answer', reasoning: 'Independent reasoning' }];
        render(<AssistantSubagents agents={agents} connected canDecide mainStopped={false} />);
        expect(screen.getByTestId('assistant-shared-transcript')).toBeInTheDocument();
        expect(screen.getByText('Answer')).toBeInTheDocument();
        expect(screen.queryByRole('dialog')).toBeNull();
        expect(screen.getByRole('textbox', { name: 'New requirements for this subtask' })).toBeInTheDocument();
    });
});
