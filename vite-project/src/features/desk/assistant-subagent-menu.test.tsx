import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { tMock } from '@/test-utils/i18n-mock';
import { AssistantSubagentMenu } from './assistant-subagents';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
vi.mock('@/hooks/use-mobile', () => ({ useIsMobile: () => true }));
afterEach(cleanup);

it('opens the shared owner approval settings without retargeting a child permission and hides unsupported approval settings', () => {
    const onApprovalSettings = vi.fn();
    const onSelect = vi.fn();
    const view = render(<AssistantSubagentMenu onSelect={onSelect} onApprovalSettings={onApprovalSettings} />);
    fireEvent.click(screen.getByRole('button', { name: tMock('pages.aiAssistant.workspace.more') }));
    fireEvent.click(screen.getByRole('button', { name: tMock('pages.aiAssistant.autoApprovalTitle') }));
    expect(onApprovalSettings).toHaveBeenCalledExactlyOnceWith();
    expect(onSelect).not.toHaveBeenCalled();
    view.rerender(<AssistantSubagentMenu onSelect={onSelect} />);
    fireEvent.click(screen.getByRole('button', { name: tMock('pages.aiAssistant.workspace.more') }));
    expect(screen.queryByRole('button', { name: tMock('pages.aiAssistant.autoApprovalTitle') })).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: tMock('pages.aiAssistant.permissionHistory') }));
    expect(onSelect).toHaveBeenCalledExactlyOnceWith('permissions');
    expect(onApprovalSettings).toHaveBeenCalledTimes(1);
});
