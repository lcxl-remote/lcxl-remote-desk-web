import { useState } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { AssistantIssueSources, parseIssueUrl } from './assistant-issue-sources';
import { getMyIssueConnections } from '@/services/clients';
import { deskErrorCodeEnum, type IssueSelection } from '@/services/types';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
vi.mock('@/services/clients', () => ({ getMyIssueConnections: vi.fn() }));
const connection = { platform: 'gitea' as const, connection_ref: 'forge', name: 'Internal forge', instance_url: 'https://forge.test/gitea/' };
const issue = { connection_ref: 'forge', owner: 'Org', repository: 'Project', issue_number: 4 };
const loaded = { success: true, code: deskErrorCodeEnum.SUCCESS, message: null, data: [connection] };
beforeEach(() => vi.mocked(getMyIssueConnections).mockReset().mockResolvedValue(loaded));

describe('issue sources', () => {
    it('keeps the configured origin and installation path while discarding URL view fragments', () => {
        expect(parseIssueUrl(connection, 'https://forge.test/gitea/Org/Project/issues/4#issuecomment-7')).toEqual(issue);
        for (const invalid of ['https://other.test/gitea/Org/Project/issues/4',
            'https://forge.test/other/Org/Project/issues/4', 'https://user:secret@forge.test/gitea/Org/Project/issues/4',
            'https://forge.test/gitea/Org/Project/pulls/4', 'https://forge.test/gitea/Org/Project/issues/0',
            'https://forge.test/gitea/Org/Project/issues/9007199254740993',
            'https://forge.test/gitea/Org/Project/issues/4/extra']) {
            expect(parseIssueUrl(connection, invalid)).toBeNull();
        }
    });

    it('selects an explicit issue without querying the external server and rejects duplicate selection', async () => {
        const change = vi.fn();
        function Controlled() {
            const [value, setValue] = useState<IssueSelection[]>([]);
            return <AssistantIssueSources value={value} onChange={next => { change(next); setValue(next); }} disabled={false} />;
        }
        render(<Controlled />);
        const url = await screen.findByLabelText('Issue URL');
        expect(screen.getByRole('combobox', { name: 'Connection' })).toHaveTextContent('Internal forge');
        fireEvent.change(url, { target: { value: 'https://forge.test/gitea/Org/Project/issues/4#issuecomment-7' } });
        fireEvent.click(screen.getByRole('button', { name: 'Add issue' }));
        expect(change).toHaveBeenCalledWith([issue]);
        expect(screen.getByText('Internal forge · Org/Project #4')).toBeInTheDocument();
        expect(url).toHaveValue('');
        fireEvent.change(url, { target: { value: 'https://forge.test/gitea/org/project/issues/4' } });
        fireEvent.click(screen.getByRole('button', { name: 'Add issue' }));
        await screen.findByText('This issue is already selected.');
        expect(change).toHaveBeenCalledTimes(1);
        expect(getMyIssueConnections).toHaveBeenCalledTimes(1);
    });

    it('preserves selected scopes after access withdrawal until the user removes them', async () => {
        const change = vi.fn();
        vi.mocked(getMyIssueConnections).mockResolvedValueOnce(loaded)
            .mockResolvedValueOnce({ ...loaded, data: [] });
        function Controlled() {
            const [value, setValue] = useState<IssueSelection[]>([issue]);
            return <AssistantIssueSources value={value} onChange={next => { change(next); setValue(next); }} disabled={false} />;
        }
        render(<Controlled />);
        await screen.findByLabelText('Issue URL');
        fireEvent.click(screen.getByRole('button', { name: 'Reload connections' }));
        await screen.findByText('No issue connections are available.');
        expect(screen.getByText('Org/Project #4')).toBeInTheDocument();
        expect(change).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'Remove Org/Project #4' }));
        await waitFor(() => expect(change).toHaveBeenCalledWith([]));
        expect(screen.queryByText('Org/Project #4')).not.toBeInTheDocument();
    });
});
