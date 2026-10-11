import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import type { ComponentType } from 'react'
import type { AxiosResponse } from 'axios'
import { axiosInstance } from '@/lib/kubb-client'
import { LogSettings } from './log-settings'
import { TurnClientSettingsPage } from './turn-client-settings'
import { AiPolicySettings } from './ai-policy-settings'
import { SystemSettings } from './system-settings'
import { DeskConnectionSettings } from './desk-connection-settings'
import { TurnSettings } from './turn-settings'

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((m) => m.reactI18nextMock()))
const h = vi.hoisted(() => ({ toast: vi.fn(), confirm: vi.fn(async () => true) }))
vi.mock('@/hooks/use-toast', () => ({ useToast: () => ({ toast: h.toast }) }))
vi.mock('@/components/confirm-dialog', () => ({ useConfirm: () => ({ confirm: h.confirm, dialog: null }) }))
vi.mock('./file-recovery-settings', () => ({ FileRecoverySettings: () => null }))
vi.mock('./computer-use-application-policy', () => ({ ComputerUseApplicationPolicySettings: () => null }))
vi.mock('./computer-use-communication-policy', () => ({ ComputerUseCommunicationPolicySettings: () => null }))
vi.mock('./manager-link-banner', () => ({ ManagerLinkBanner: () => null }))
vi.mock('./backend-diagnostics-card', () => ({ BackendDiagnosticsCard: () => null }))
vi.mock('./turn-runtime-status', () => ({ TurnRuntimeStatus: () => null }))
vi.mock('@/features/layout/service-install-dialog', () => ({ ServiceInstallDialog: () => null }))
vi.mock('@/features/layout/service-uninstall-dialog', () => ({ ServiceUninstallDialog: () => null }))

type Snapshot = Record<string, unknown>
type Scenario = {
    name: string
    component: ComponentType
    url: string
    edit: () => void
    assertSaved: () => void
    patch: Snapshot
    saveIndex?: number
}

const url = (section = '') => `/api/desk/settings${section ? `/${section}` : ''}`
const change = (label: string, value: string) => fireEvent.change(screen.getByLabelText(label), { target: { value } })
const scenarios: Scenario[] = [
    {
        name: 'log settings', component: LogSettings, url: url('log'),
        edit: () => change('Log Retention Days', '14'),
        assertSaved: () => expect(screen.getByLabelText('Log Retention Days')).toHaveValue(14),
        patch: { log_retention_days: 14 },
    },
    {
        name: 'TURN client settings', component: TurnClientSettingsPage, url: url('turn-client'),
        edit: () => {
            fireEvent.keyDown(screen.getByRole('combobox'), { key: 'ArrowDown' })
            fireEvent.click(screen.getByRole('option', { name: 'STUN (P2P)' }))
        },
        assertSaved: () => expect(screen.getByRole('combobox')).toHaveTextContent('STUN (P2P)'),
        patch: { traversal_mode: 'stun' },
    },
    {
        name: 'AI execution policy', component: AiPolicySettings, url: url('ai-policy'),
        edit: () => change('Concurrent command limit', '9'),
        assertSaved: () => expect(screen.getByLabelText('Concurrent command limit')).toHaveValue(9),
        patch: { max_concurrent_executions: 9 },
    },
    {
        name: 'evidence collection policy', component: AiPolicySettings, url: url('collection-policy'), saveIndex: 1,
        edit: () => fireEvent.click(screen.getByRole('switch', { name: 'Allow logs' })),
        assertSaved: () => expect(screen.getByRole('switch', { name: 'Allow logs' })).toBeChecked(),
        patch: { allow_logs: true },
    },
    {
        name: 'system settings', component: SystemSettings, url: url(),
        edit: () => change('Port number (takes effect after restart)', '8181'),
        assertSaved: () => expect(screen.getByLabelText('Port number (takes effect after restart)')).toHaveValue(8181),
        patch: { port: 8181 },
    },
    {
        name: 'desk connection settings', component: DeskConnectionSettings, url: url(),
        edit: () => change('Remote Signaling Server URL', 'ws://signal.example/api/desk/signaling'),
        assertSaved: () => expect(screen.getByLabelText('Remote Signaling Server URL')).toHaveValue('ws://signal.example/api/desk/signaling'),
        patch: { signaling_url: 'ws://signal.example/api/desk/signaling' },
    },
    {
        name: 'TURN server settings', component: TurnSettings, url: url('turn'),
        edit: () => change('Realm', 'updated.example'),
        assertSaved: () => expect(screen.getByLabelText('Realm')).toHaveValue('updated.example'),
        patch: { realm: 'updated.example' },
    },
]

let client: QueryClient
let saved: Record<string, Snapshot>
let writes: { url: string; data: Snapshot }[]
let rejectWrite: boolean
let rejectRefresh: boolean
let readsAfterWrite: Promise<void> | undefined
let request: ReturnType<typeof vi.spyOn>

beforeEach(() => {
    client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: Infinity }, mutations: { retry: false } } })
    saved = {
        [url()]: { port: 8081, enable_ipv6: true, auto_start: false, listen_addr_ipv4: '0.0.0.0', listen_addr_ipv6: '::', host_access_indicator_enabled: true, manager_enabled: true, require_secure_signaling: true, worker_heartbeat_timeout_ms: 5000 },
        [url('log')]: { log_level: 'info', traceback: true, log_retention_days: 7, log_cleanup_threshold_percent: 90, log_cleanup_interval_hours: 12 },
        [url('turn-client')]: { traversal_mode: 'turn' },
        [url('ai-policy')]: { execution_mode: 'confirm_each_action', max_concurrent_executions: 4, max_command_runtime_seconds: 600, exec_pty_enabled: true, interactive_elevation_enabled: false, exec_pty_supported: true, interactive_elevation_supported: false },
        [url('collection-policy')]: { allow_logs: false, allow_screen: true },
        [url('ai-assistant')]: { enabled: true, revision: '1' },
        [url('turn')]: { realm: 'initial.example', enable_turn: true, interfaces: [{ transport: 'udp', listen: '0.0.0.0:3478', external: '127.0.0.1:3478' }], relay_min_port: 50000, relay_max_port: 50050, static_auth_secret: 'initial-secret' },
    }
    writes = []
    rejectWrite = false
    rejectRefresh = false
    readsAfterWrite = undefined
    h.toast.mockClear()
    h.confirm.mockClear()
    // Exercise the real generated query and mutation hooks against a fake server.
    request = vi.spyOn(axiosInstance, 'request').mockImplementation(async (config) => {
        const path = config.url!
        if (config.method === 'POST') {
            if (rejectWrite) throw new Error('save failed')
            const data = config.data as Snapshot
            writes.push({ url: path, data })
            if (path === '/api/desk/settings/turn/regenerate-secret') {
                saved[url('turn')].static_auth_secret = 'rotated-secret'
            } else {
                saved[path] = { ...saved[path], ...data }
            }
            return { data: undefined } as AxiosResponse
        }
        const snapshot = structuredClone(saved[path] ?? {})
        if (rejectRefresh && writes.some((write) => write.url === path)) throw new Error('refresh failed')
        if (readsAfterWrite && writes.some((write) => write.url === path)) await readsAfterWrite
        return { data: { success: true, data: snapshot } } as AxiosResponse
    })
})

afterEach(() => {
    client.clear()
    request.mockRestore()
})

function mount(Component: ComponentType) {
    return render(<QueryClientProvider client={client}><Component /></QueryClientProvider>)
}

function saveButton(scenario: Scenario) {
    return screen.getAllByRole('button', { name: /^Save(?: Settings)?$/ })[scenario.saveIndex ?? 0]
}

describe.each(scenarios)('$name cache after saving', (scenario) => {
    it('returns to the page with the saved configuration and preserves unrelated fields', async () => {
        const view = mount(scenario.component)
        await screen.findAllByRole('button', { name: /^Save(?: Settings)?$/ })
        scenario.edit()
        fireEvent.click(saveButton(scenario))
        await waitFor(() => expect(h.toast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Success' })))
        expect(client.getQueryData([{ url: scenario.url }])).toMatchObject({ data: scenario.patch })
        expect(writes).toHaveLength(1)
        if (scenario.url === url()) expect(writes[0].data.worker_heartbeat_timeout_ms).toBe(5000)
        view.unmount()
        mount(scenario.component)
        await waitFor(scenario.assertSaved)
    })

    it('keeps saving disabled until the saved snapshot is refreshed', async () => {
        mount(scenario.component)
        await screen.findAllByRole('button', { name: /^Save(?: Settings)?$/ })
        scenario.edit()
        let finishRead!: () => void
        readsAfterWrite = new Promise((resolve) => { finishRead = resolve })
        fireEvent.click(saveButton(scenario))
        await waitFor(() => expect(writes).toHaveLength(1))
        expect(saveButton(scenario)).toBeDisabled()
        fireEvent.click(saveButton(scenario))
        expect(writes).toHaveLength(1)
        expect(h.toast).not.toHaveBeenCalled()
        await act(async () => { finishRead() })
        await waitFor(() => expect(saveButton(scenario)).toBeEnabled())
        expect(client.getQueryData([{ url: scenario.url }])).toMatchObject({ data: scenario.patch })
    })

    it('preserves the draft and existing cache when saving fails', async () => {
        mount(scenario.component)
        await screen.findAllByRole('button', { name: /^Save(?: Settings)?$/ })
        const previous = client.getQueryData([{ url: scenario.url }])
        scenario.edit()
        rejectWrite = true
        fireEvent.click(saveButton(scenario))
        await waitFor(() => expect(h.toast).toHaveBeenCalledWith(expect.objectContaining({ variant: 'destructive' })))
        expect(client.getQueryData([{ url: scenario.url }])).toEqual(previous)
        scenario.assertSaved()
        expect(writes).toHaveLength(0)
    })

    it('reports a failed refresh separately from a successful save', async () => {
        mount(scenario.component)
        await screen.findAllByRole('button', { name: /^Save(?: Settings)?$/ })
        scenario.edit()
        rejectRefresh = true
        fireEvent.click(saveButton(scenario))
        await waitFor(() => expect(h.toast).toHaveBeenCalledWith(expect.objectContaining({
            description: 'Settings were saved, but refreshing the configuration failed. Reload the page to confirm the saved values.',
        })))
        expect(saved[scenario.url]).toMatchObject(scenario.patch)
        scenario.assertSaved()
        expect(saveButton(scenario)).toBeEnabled()
        expect(writes).toHaveLength(1)
    })
})

it('updates an untouched form when a fresh query replaces a cached snapshot', async () => {
    client.setDefaultOptions({ queries: { retry: false, staleTime: 0 } })
    client.setQueryData([{ url: url('log') }], { success: true, data: { ...saved[url('log')] } })
    saved[url('log')].log_retention_days = 30
    mount(LogSettings)
    await waitFor(() => expect(screen.getByLabelText('Log Retention Days')).toHaveValue(30))
})

it('does not replace edits made while a saved snapshot is being refreshed', async () => {
    mount(LogSettings)
    await screen.findByLabelText('Log Retention Days')
    change('Log Retention Days', '14')
    let finishRead!: () => void
    readsAfterWrite = new Promise((resolve) => { finishRead = resolve })
    fireEvent.click(screen.getByRole('button', { name: 'Save Settings' }))
    await waitFor(() => expect(writes).toHaveLength(1))
    change('Log Retention Days', '21')
    await act(async () => { finishRead() })
    await waitFor(() => expect(h.toast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Success' })))
    expect(screen.getByLabelText('Log Retention Days')).toHaveValue(21)
    expect(client.getQueryData([{ url: url('log') }])).toMatchObject({ data: { log_retention_days: 14 } })
})

it('refreshes the configuration cache after rotating the TURN secret', async () => {
    mount(TurnSettings)
    fireEvent.click(await screen.findByRole('button', { name: 'Regenerate TURN Secret' }))
    fireEvent.click(await screen.findByRole('button', { name: 'Confirm' }))
    await waitFor(() => expect(h.toast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Success' })))
    expect(client.getQueryData([{ url: url('turn') }])).toMatchObject({ data: { static_auth_secret: 'rotated-secret' } })
    change('Realm', 'after-rotation.example')
    fireEvent.click(screen.getByRole('button', { name: 'Save Settings' }))
    await waitFor(() => expect(writes).toHaveLength(2))
    expect(writes[1].data.static_auth_secret).toBe('rotated-secret')
})
