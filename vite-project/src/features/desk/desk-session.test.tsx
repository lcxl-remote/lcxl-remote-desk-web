import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';
import { describe, it, expect, vi } from 'vitest';
import DeskSession from './desk-session';
import React from 'react';

// Mock routing
vi.mock('react-router-dom', () => ({
  useParams: () => ({ id: 'test-desk' }),
  useNavigate: () => vi.fn(),
}));

// Mock translations
vi.mock('react-i18next', () => ({
  useTranslation: () => ({
    t: (key: string, fallback: string) => fallback,
  }),
}));

// Mock hooks
vi.mock('./use-desk-signaling', () => ({
  useDeskSignaling: () => ({
    isConnected: true, // Force connected state to show control bar
    lastMessage: null,
    sendMessage: vi.fn(),
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
  }),
}));

vi.mock('./use-desk-clipboard', () => ({
  useDeskClipboard: () => ({
    clipboardEnabled: false,
    transferStatus: 'idle',
    fallbackToast: { show: false },
  }),
}));

vi.mock('./use-desk-whiteboard', () => ({
  useDeskWhiteboard: () => ({
    isActive: false,
    canActivate: true,
    elements: [],
    toggleWhiteboard: vi.fn(),
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

// Mock ResizeObserver
globalThis.ResizeObserver = class ResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
} as any;

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