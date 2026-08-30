import { act, render, screen, fireEvent, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';
import { describe, it, expect, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import DeskSession from './desk-session';
import {
  armControlReconnect,
  armSettingsApplyTimeout,
  buildDesktopRequestRemotePayload,
  claimControlReconnect,
  effectiveVideoQuality,
  isRemoteSettingsFailure,
  resolveAdaptiveBitrateForHost,
  shouldOpenConfigDialog,
  shouldShowMediaPipelineOverlay,
  systemAudioStateTranslationKey,
  videoSettingsMayInterrupt,
  waitForVideoPresentation,
} from './desk-session-model';
import React from 'react';
import { SIGNALING_TYPE_CODE_MEDIA_PIPELINE_STATE_CHANGED } from './constants';

const signalingHarness = vi.hoisted(() => ({
  subscribers: new Set<(message: any) => void>(),
}));

describe('settings apply model', () => {
  it('keeps an unsupported host adaptive-bitrate setting at its suggestion', () => {
    expect(resolveAdaptiveBitrateForHost('unsupported', false, true)).toBe(false);
    expect(resolveAdaptiveBitrateForHost('apply', false, true)).toBe(true);
  });

  it('replaces the offline deadline when the request reaches the wire', () => {
    vi.useFakeTimers();
    const pending = { requestId: 'settings-1', timer: null as number | null };
    const queuedTimeout = vi.fn();
    const responseTimeout = vi.fn();

    expect(armSettingsApplyTimeout(pending, 'settings-1', queuedTimeout, 1_000)).toBe(true);
    vi.advanceTimersByTime(500);
    expect(armSettingsApplyTimeout(pending, 'settings-1', responseTimeout, 1_000)).toBe(true);
    vi.advanceTimersByTime(500);
    expect(queuedTimeout).not.toHaveBeenCalled();
    expect(responseTimeout).not.toHaveBeenCalled();
    vi.advanceTimersByTime(500);
    expect(responseTimeout).toHaveBeenCalledTimes(1);
    vi.useRealTimers();
  });

  it('localizes every system-audio state and has an unknown fallback', () => {
    expect(systemAudioStateTranslationKey('off')).toBe('pages.desk.systemAudioStateValue.off');
    expect(systemAudioStateTranslationKey('starting')).toBe('pages.desk.systemAudioStateValue.starting');
    expect(systemAudioStateTranslationKey('active')).toBe('pages.desk.systemAudioStateValue.active');
    expect(systemAudioStateTranslationKey('restarting')).toBe('pages.desk.systemAudioStateValue.restarting');
    expect(systemAudioStateTranslationKey('denied')).toBe('pages.desk.systemAudioStateValue.denied');
    expect(systemAudioStateTranslationKey('failed')).toBe('pages.desk.systemAudioStateValue.failed');
    expect(systemAudioStateTranslationKey('future-state')).toBe('pages.desk.systemAudioStateValue.unknown');
  });
});

describe('settings reconnect control restoration', () => {
  it('does not carry authorization into the replacement epoch', () => {
    const intent = armControlReconnect(true, 'epoch-1');

    expect(claimControlReconnect(intent, 'epoch-1', true)).toEqual({
      intent,
      shouldRequest: false,
    });
    expect(claimControlReconnect(intent, 'epoch-2', false)).toEqual({
      intent,
      shouldRequest: false,
    });
  });

  it('requests control once the replacement PC connects', () => {
    const intent = armControlReconnect(true, 'epoch-1');

    expect(claimControlReconnect(intent, 'epoch-2', true)).toEqual({
      intent: null,
      shouldRequest: true,
    });
    expect(armControlReconnect(false, 'epoch-1')).toBeNull();
  });
});

// Mock routing
vi.mock('react-router-dom', () => ({
  useParams: () => ({ id: 'test-desk' }),
  useNavigate: () => vi.fn(),
}));

// Mock translations
vi.mock('react-i18next', () => ({
  useTranslation: () => ({
    t: (key: string, fallback?: unknown) =>
      typeof fallback === 'string' ? fallback : key,
  }),
}));

// Mock hooks
vi.mock('./use-desk-signaling', () => ({
  useDeskSignaling: () => ({
    isConnected: true, // Force connected state to show control bar
    subscribe: (handler: (message: any) => void) => {
      signalingHarness.subscribers.add(handler);
      return () => signalingHarness.subscribers.delete(handler);
    },
    sendMessage: vi.fn(),
    sendTracked: vi.fn(() => ({ requestId: 'r', disposition: 'sent' })),
    cancelQueued: vi.fn(),
  }),
}));

vi.mock('./use-desk-rtc', () => ({
  useDeskRTC: () => ({
    isRTCConnected: true,
    rtcStats: { fps: 60, bitrate: 1000 },
    connect: vi.fn(),
    closeRTC: vi.fn(),
  }),
}));

vi.mock('./use-desk-input', () => ({
  useDeskInput: () => ({
    sendKeyboardEvents: vi.fn(),
    releaseAllInputs: vi.fn(),
  }),
}));

vi.mock('./use-desk-clipboard', () => ({
  useDeskClipboard: () => ({
    clipboardEnabled: false,
    transferStatus: 'idle',
    fallbackToast: { show: false },
    toggleClipboard: vi.fn(),
    enableClipboard: vi.fn(),
  }),
}));

vi.mock('./use-desk-whiteboard', () => ({
  useDeskWhiteboard: () => ({
    isActive: false,
    isInteractive: false,
    canActivate: true,
    elements: [],
    toggleWhiteboard: vi.fn(),
    deactivateWhiteboard: vi.fn(),
  }),
}));

vi.mock('./use-desk-microphone', () => ({
  useDeskMicrophone: () => ({
    isMicActive: false,
    toggleMicrophone: vi.fn(),
  }),
}));

vi.mock('./use-cursor-sync', () => ({
  useCursorSync: () => ({
    cursorStyle: 'default',
  }),
}));

vi.mock('@/hooks/use-device-id', () => ({
  useDeviceConnectionResolution: () => ({
    status: 'persistent',
    connection: {
      connection_id: 'test-desk',
      version_info: { client_id: 'standalone-test-host' },
    },
    deviceKey: 'client:standalone-test-host',
  }),
}));

// Mock ResizeObserver
globalThis.ResizeObserver = class ResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
} as any;

declare const require: any;

// Mock Popover to reliably test onOpenChange
vi.mock('@/components/ui/popover', () => {
  const React = require('react');
  return {
    Popover: ({ children, onOpenChange }: any) => (
      <div 
        data-testid="mock-popover" 
        onClick={() => onOpenChange?.(true)}
      >
        {children}
      </div>
    ),
    PopoverTrigger: ({ children, asChild }: any) => {
      if (asChild) {
        return React.cloneElement(children, { 'data-popover-trigger': 'true' });
      }
      return <button data-popover-trigger="true">{children}</button>;
    },
    PopoverContent: ({ children }: any) => <div>{children}</div>,
  };
});

// Mock DropdownMenu to prevent real Radix from firing onOpenChange(false) on blur
vi.mock('@/components/ui/dropdown-menu', () => {
  return {
    DropdownMenu: ({ children }: any) => <div>{children}</div>,
    DropdownMenuTrigger: ({ children, asChild }: any) => {
      const React = require('react');
      if (asChild) return React.cloneElement(children, { 'data-dropdown-trigger': 'true' });
      return <button>{children}</button>;
    },
    DropdownMenuContent: ({ children }: any) => <div>{children}</div>,
    DropdownMenuItem: ({ children }: any) => <div>{children}</div>,
  };
});

describe('DeskSession Control Bar', () => {
  it('should initially be collapsed', () => {
    const { container } = render(<DeskSession />);
    
    // Find the content container
    const content = container.querySelector('.controlBarContent');
    expect(content).toBeInTheDocument();
    
    // Check initial state
    expect(content).toHaveClass('collapsed');
    expect(content).not.toHaveClass('expanded');
    expect(content).toHaveAttribute('inert');
  });

  it('should expand on mouse enter and collapse on mouse leave', async () => {
    const { container } = render(<DeskSession />);
    
    const dragHandle = container.querySelector('.controlBarDragHandle');
    const content = container.querySelector('.controlBarContent');
    
    expect(dragHandle).toBeInTheDocument();
    
    // Hover over the handle
    fireEvent.mouseEnter(dragHandle!);
    
    await waitFor(() => {
      expect(content).toHaveClass('expanded');
      expect(content).not.toHaveAttribute('inert');
    });
    
    // Leave the handle
    fireEvent.mouseLeave(dragHandle!);
    
    await waitFor(() => {
      expect(content).toHaveClass('collapsed');
      expect(content).toHaveAttribute('inert');
    });
  });

  it('should remain expanded when a menu is open', async () => {
    const { container } = render(<DeskSession />);
    
    const content = container.querySelector('.controlBarContent');
    
    // Focus the content to expand it
    fireEvent.focus(content!);
    
    await waitFor(() => {
      expect(content).toHaveClass('expanded');
    });
    
    // Open the mock popover directly
    const mockPopover = content!.querySelector('[data-testid="mock-popover"]');
    expect(mockPopover).not.toBeNull();
    fireEvent.click(mockPopover!);
    
    await waitFor(() => {
      // Ensure it remains expanded
      expect(content).toHaveClass('expanded');
    });
    
    // Even if we blur or mouse leave, it should stay expanded because the menu sets the state
    fireEvent.mouseLeave(content!);

    // We expect it to still be expanded
    expect(content).toHaveClass('expanded');
  });
});

describe('effectiveVideoQuality', () => {
  it('reports the adaptive override once the loop has stepped the encoder', () => {
    // The host never echoes a new baseline back for an adaptive step, so the
    // override is the only record that the encoder moved. Reading the baseline
    // alone froze the panel at the session-start value.
    expect(effectiveVideoQuality(37, 22)).toBe(37);
  });

  it('falls back to the host-accepted baseline while no override is in force', () => {
    expect(effectiveVideoQuality(null, 22)).toBe(22);
  });

  it('reports nothing when neither an override nor a baseline exists', () => {
    expect(effectiveVideoQuality(null, null)).toBeNull();
    expect(effectiveVideoQuality(null, undefined)).toBeNull();
  });

  it('keeps a zero override, which is a valid (best) quality value', () => {
    expect(effectiveVideoQuality(0, 22)).toBe(0);
  });
});

describe('shouldOpenConfigDialog', () => {
  const base = {
    hasInitData: true,
    isRTCConnected: false,
    hasAttemptedConnect: false,
    rtcFailed: false,
  };

  it('opens for the initial settings pick (REMOTE_ACCESS_INITIALIZED arrived, no attempt yet)', () => {
    expect(shouldOpenConfigDialog(base)).toBe(true);
  });

  it('stays closed before REMOTE_ACCESS_INITIALIZED data arrives', () => {
    expect(shouldOpenConfigDialog({ ...base, hasInitData: false })).toBe(false);
  });

  it('stays closed while connected', () => {
    expect(shouldOpenConfigDialog({ ...base, isRTCConnected: true })).toBe(false);
  });

  it('does NOT reopen on a transient disconnect after a connect attempt', () => {
    // The reported flapping: attempted a connect, ICE dipped to `disconnected`
    // (isRTCConnected false) but not `failed`. Reopening here would strand the
    // recovering video behind the dialog.
    expect(
      shouldOpenConfigDialog({
        ...base,
        hasAttemptedConnect: true,
        rtcFailed: false,
      }),
    ).toBe(false);
  });

  it('reopens after a terminal ICE failure so the user can retry', () => {
    expect(
      shouldOpenConfigDialog({
        ...base,
        hasAttemptedConnect: true,
        rtcFailed: true,
      }),
    ).toBe(true);
  });
});

describe('shouldShowMediaPipelineOverlay', () => {
  it('hides the blocked warning while the encoder dialog is open', () => {
    expect(shouldShowMediaPipelineOverlay(true, false)).toBe(true);
    expect(shouldShowMediaPipelineOverlay(true, true)).toBe(false);
  });

  it('does not show an overlay without pipeline state', () => {
    expect(shouldShowMediaPipelineOverlay(false, false)).toBe(false);
  });
});

describe('isRemoteSettingsFailure', () => {
  it('accepts a typed success response whose response_state has code zero', () => {
    expect(isRemoteSettingsFailure(false, 0)).toBe(false);
  });

  it('rejects both typed failures and correlated protocol Error frames', () => {
    expect(isRemoteSettingsFailure(false, 4)).toBe(true);
    expect(isRemoteSettingsFailure(true, 0)).toBe(true);
  });
});

describe('videoSettingsMayInterrupt', () => {
  const baseline = {
    adaptive_bitrate: true,
    audio: null,
    enable_dirty_rect: true,
    image_capture: 'WGC',
    show_mouse: true,
    video_device_name: 'DISPLAY1',
    video_encoder: 'X264' as const,
    video_fps: 60,
    video_quality: 22,
  };

  it('covers capture, display and encoder replacement', () => {
    expect(videoSettingsMayInterrupt(baseline, { ...baseline, image_capture: 'DXGI' })).toBe(true);
    expect(videoSettingsMayInterrupt(baseline, { ...baseline, video_device_name: 'DISPLAY2' })).toBe(true);
    expect(videoSettingsMayInterrupt(baseline, { ...baseline, video_encoder: 'VP8' })).toBe(true);
  });

  it('does not cover settings applied to the current video generation', () => {
    expect(videoSettingsMayInterrupt(baseline, { ...baseline, video_fps: 30 })).toBe(false);
    expect(videoSettingsMayInterrupt(baseline, { ...baseline, video_quality: 30 })).toBe(false);
    expect(videoSettingsMayInterrupt(baseline, { ...baseline, show_mouse: false })).toBe(false);
  });
});

describe('waitForVideoPresentation', () => {
  it('uses the browser video-frame callback when available', () => {
    const video = document.createElement('video');
    let callback: (() => void) | null = null;
    video.requestVideoFrameCallback = vi.fn((next) => {
      callback = next as unknown as () => void;
      return 7;
    });
    video.cancelVideoFrameCallback = vi.fn();
    const presented = vi.fn();

    waitForVideoPresentation(video, presented);
    expect(presented).not.toHaveBeenCalled();
    expect(callback).not.toBeNull();
    callback!();
    expect(presented).toHaveBeenCalledTimes(1);
  });

  it('finishes early when playback quality reports another frame', () => {
    vi.useFakeTimers();
    const video = document.createElement('video');
    let totalVideoFrames = 10;
    video.getVideoPlaybackQuality = () => ({
      corruptedVideoFrames: 0,
      creationTime: 0,
      droppedVideoFrames: 0,
      totalVideoFrames,
    });
    const presented = vi.fn();

    waitForVideoPresentation(video, presented);
    totalVideoFrames = 11;
    vi.advanceTimersByTime(100);
    expect(presented).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(5_000);
    expect(presented).toHaveBeenCalledTimes(1);
    vi.useRealTimers();
  });
});

describe('buildDesktopRequestRemotePayload', () => {
  it('always identifies desktop sessions explicitly', () => {
    expect(buildDesktopRequestRemotePayload('desk-1', null, 'auto')).toEqual({
      connection_id: 'desk-1',
      purpose: 'remote_desktop',
      requested_wayland_control_mode: 'auto',
    });
    expect(buildDesktopRequestRemotePayload('desk-1', 'grant-1', 'uinput')).toEqual({
      connection_id: 'desk-1',
      purpose: 'remote_desktop',
      requested_wayland_control_mode: 'uinput',
      grant_session_id: 'grant-1',
    });
    expect(buildDesktopRequestRemotePayload('desk-1', null, 'auto', 9)).toEqual({
      connection_id: 'desk-1',
      purpose: 'remote_desktop',
      requested_wayland_control_mode: 'auto',
      org_id: 9,
    });
    expect(buildDesktopRequestRemotePayload('desk-1', 'grant-1', 'auto', 9)).toEqual({
      connection_id: 'desk-1',
      purpose: 'remote_desktop',
      requested_wayland_control_mode: 'auto',
      grant_session_id: 'grant-1',
    });
    expect(buildDesktopRequestRemotePayload('desk-1', null, 'auto', undefined, 'target-1')).toEqual({
      connection_id: 'desk-1',
      purpose: 'remote_desktop',
      requested_wayland_control_mode: 'auto',
      session_target_id: 'target-1',
    });
  });
});

describe('DeskSession Video Sizing', () => {
  it('shows the shared animated loading treatment before the first video frame', () => {
    const { container } = render(<DeskSession />);

    expect(container.querySelector('.videoPlaceholder')).not.toHaveClass('hidden');
    expect(container.querySelector('.videoLoadingBar')).toBeInTheDocument();
    expect(screen.getByText('pages.desk.connectingVideo')).toBeInTheDocument();
  });

  it('keeps the video element using object-contain so it letterboxes inside the wrapper', () => {
    const { container } = render(<DeskSession />);

    const video = container.querySelector('video.videoElement');
    expect(video).toBeInTheDocument();
    // h-full + w-full + object-contain is what makes the video letterbox
    // instead of overflowing the short axis. Guard against accidental
    // removal — that regression produced a scrollbar in short viewports.
    expect(video).toHaveClass('h-full');
    expect(video).toHaveClass('w-full');
    expect(video).toHaveClass('object-contain');
  });

  it('positions .videoWrapper absolutely so its size does not depend on a height: 100% chain', () => {
    const { container } = render(<DeskSession />);

    const wrapper = container.querySelector('.videoWrapper') as HTMLElement | null;
    expect(wrapper).not.toBeNull();

    // jsdom does not apply imported CSS to document.styleSheets, so read
    // the file from disk and assert the rule directly. Catches anyone
    // reverting .videoWrapper back to height: 100%, which is what allowed
    // the video's intrinsic ratio to overflow the container.
    // Vitest cwd is the vite-project root.
    const cssPath = resolve(process.cwd(), 'src/features/desk/desk-session.css');
    // Strip /* ... */ block comments so they don't trip the assertions.
    const css = readFileSync(cssPath, 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
    const wrapperBlock = css.match(/\.videoWrapper\s*{([^}]*)}/);
    expect(wrapperBlock, '.videoWrapper rule must exist in desk-session.css').toBeTruthy();
    expect(wrapperBlock![1]).toMatch(/position\s*:\s*absolute/);
    expect(wrapperBlock![1]).toMatch(/inset\s*:\s*0/);
    expect(wrapperBlock![1]).not.toMatch(/height\s*:\s*100%/);
  });

  it('does not put the black placeholder back over an already-ready video during pipeline recovery', async () => {
    const { container } = render(<DeskSession />);
    const video = container.querySelector('video.videoElement');
    const placeholder = container.querySelector('.videoPlaceholder');
    expect(video).not.toBeNull();
    expect(placeholder).not.toBeNull();

    fireEvent.canPlay(video!);
    expect(placeholder).toHaveClass('hidden');

    await act(async () => {
      [...signalingHarness.subscribers].forEach((handler) => handler({
        signaling_type: SIGNALING_TYPE_CODE_MEDIA_PIPELINE_STATE_CHANGED,
        signaling_data: {
          phase: 'blocked',
          reason_code: 1,
          compatible_encoders: ['X264'],
        },
      }));
    });
    expect(placeholder).toHaveClass('hidden');

    await act(async () => {
      [...signalingHarness.subscribers].forEach((handler) => handler({
        signaling_type: SIGNALING_TYPE_CODE_MEDIA_PIPELINE_STATE_CHANGED,
        signaling_data: {
          phase: 'streaming',
          encoder: 'X264',
          compatible_encoders: [],
        },
      }));
    });
    expect(placeholder).toHaveClass('hidden');
  });
});
