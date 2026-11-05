import { afterEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { IssueConnectionsSettings } from './issue-connections-settings';
import { deskErrorCodeEnum } from '@/services/types';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

const config = {
    revision: 4,
    connections: [{
        platform: 'gitea', connection_ref: 'forge', name: 'Internal Gitea', enabled: true,
        instance_url: 'https://forge.test/', mcp_url: 'https://mcp.test/mcp',
        allowed_actor_ids: ['1'], has_token: true, has_custom_ca: true,
    }],
};
const path = '/api/admin/system/issue-connections';
function response(data: unknown) {
    return new Response(JSON.stringify({ success: true, data }), {
        headers: { 'Content-Type': 'application/json' },
    });
}
afterEach(() => vi.unstubAllGlobals());

describe('external issue connection settings', () => {
    it('saves the selected platform with its replacement credential', async () => {
        const fetch = vi.fn().mockResolvedValueOnce(response(config))
            .mockResolvedValueOnce(response({ ...config, revision: 5 }));
        vi.stubGlobal('fetch', fetch);
        render(<IssueConnectionsSettings />);
        const platform = await screen.findByRole('combobox', { name: 'Platform' });
        fireEvent.click(platform);
        fireEvent.click(await screen.findByRole('option', { name: 'GitHub' }));
        fireEvent.change(screen.getByLabelText('Access token'), { target: { value: 'replacement' } });
        fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await screen.findByText('Connections saved');
        expect(JSON.parse(fetch.mock.calls[1][1].body).connections[0])
            .toEqual(expect.objectContaining({ platform: 'github', token: 'replacement' }));
    });
    it('retains editable drafts after a definite validation rejection', async () => {
        const fetch = vi.fn().mockResolvedValueOnce(response(config))
            .mockResolvedValueOnce(new Response(JSON.stringify({ success:false, code:deskErrorCodeEnum.INVALID_PARAMS })))
            .mockResolvedValueOnce(response({ ...config, revision:5 }));
        vi.stubGlobal('fetch', fetch);
        render(<IssueConnectionsSettings />);
        fireEvent.change(await screen.findByLabelText('Access token'), { target:{ value:'new-secret' } });
        fireEvent.change(screen.getByLabelText('Instance URL'), { target:{ value:'invalid' } });
        fireEvent.click(screen.getByRole('button', { name:'Save' }));
        await screen.findByRole('alert');
        expect(screen.getByLabelText('Access token')).toHaveValue('new-secret');
        expect(screen.getByLabelText('Instance URL')).toHaveValue('invalid');
        expect(fetch).toHaveBeenCalledTimes(2);
        fireEvent.change(screen.getByLabelText('Instance URL'), { target:{ value:'https://new-forge.test/' } });
        fireEvent.click(screen.getByRole('button', { name:'Save' }));
        await screen.findByText('Connections saved');
        expect(screen.getByLabelText('Access token')).toHaveValue('');
        expect(JSON.parse(fetch.mock.calls[2][1].body)).toEqual(expect.objectContaining({ expected_revision:4 }));
    });
    it('keeps saved credentials private and preserves them when saving other fields', async () => {
        const fetch = vi.fn().mockResolvedValueOnce(response(config))
            .mockResolvedValueOnce(response({ ...config, revision: 5 }));
        vi.stubGlobal('fetch', fetch);
        render(<IssueConnectionsSettings />);
        const token = await screen.findByLabelText('Access token');
        expect(token).toHaveValue('');
        expect(token).toHaveAttribute('type', 'password');
        expect(token).toHaveAttribute('placeholder', 'Leave blank to keep the saved token');
        expect(fetch).toHaveBeenCalledTimes(1);
        fireEvent.change(token, { target: { value: 'temporary-secret' } });
        fireEvent.change(token, { target: { value: '' } });
        fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'Renamed' } });
        fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await screen.findByText('Connections saved');
        expect(fetch).toHaveBeenCalledTimes(2);
        const [endpoint, init] = fetch.mock.calls[1];
        expect(endpoint).toBe(path);
        expect(init.method).toBe('PUT');
        expect(JSON.parse(init.body)).toEqual({ expected_revision: 4, connections: [{
            platform: 'gitea', connection_ref: 'forge', name: 'Renamed', enabled: true,
            instance_url: 'https://forge.test/', mcp_url: 'https://mcp.test/mcp',
            allowed_actor_ids: ['1'], token: null, extra_ca_pem: null,
        }] });
        expect(init.body).not.toContain('temporary-secret');
    });

    it('probes only saved settings and never saves implicitly', async () => {
        const fetch = vi.fn().mockResolvedValueOnce(response(config)).mockResolvedValueOnce(response({
            account_id: 3, account_login: 'issue-bot', can_read_issues: true, can_reply: false,
        }));
        vi.stubGlobal('fetch', fetch);
        render(<IssueConnectionsSettings />);
        const test = await screen.findByRole('button', { name: 'Test connection' });
        fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'Unsaved' } });
        expect(test).toBeDisabled();
        fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'Internal Gitea' } });
        expect(test).toBeEnabled();
        fireEvent.click(test);
        fireEvent.click(test);
        await screen.findByText('Account issue-bot · Issue reads available · Replies unavailable');
        expect(fetch).toHaveBeenCalledTimes(2);
        expect(fetch.mock.calls[1]).toEqual([`${path}/forge/test`, expect.objectContaining({ method: 'POST' })]);
    });

    it('clears unsaved secrets after an uncertain save and requires reload', async () => {
        const fetch = vi.fn().mockResolvedValueOnce(response(config))
            .mockRejectedValueOnce(new Error('connection lost'))
            .mockResolvedValueOnce(response({ ...config, revision: 5 }));
        vi.stubGlobal('fetch', fetch);
        render(<IssueConnectionsSettings />);
        fireEvent.change(await screen.findByLabelText('Access token'), { target: { value: 'new-secret' } });
        fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await screen.findByRole('alert');
        expect(screen.queryByLabelText('Access token')).not.toBeInTheDocument();
        expect(screen.queryByRole('button', { name: 'Save' })).not.toBeInTheDocument();
        expect(fetch).toHaveBeenCalledTimes(2);
        fireEvent.click(screen.getByRole('button', { name: 'Reload' }));
        await waitFor(() => expect(screen.getByLabelText('Access token')).toHaveValue(''));
        expect(fetch).toHaveBeenCalledTimes(3);
        expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
    });
});
