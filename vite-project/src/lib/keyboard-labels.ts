type KeyboardPlatformInfo = Pick<Navigator, 'platform' | 'userAgent'> & {
    userAgentData?: { platform?: string };
};

/** Format shortcut labels for the local keyboard, independently of the remote device. */
export function getLocalKeyboardLabels(
    navigatorInfo: KeyboardPlatformInfo | undefined = typeof navigator === 'undefined' ? undefined : navigator,
) {
    const platform = navigatorInfo?.userAgentData?.platform || navigatorInfo?.platform || navigatorInfo?.userAgent || '';
    const isMac = /mac/i.test(platform);
    const description = {
        enter: isMac ? 'Return' : 'Enter',
        shiftEnter: isMac ? 'Shift+Return' : 'Shift+Enter',
        primaryEnter: isMac ? 'Command+Return' : 'Ctrl+Enter',
    };
    return {
        display: isMac ? { enter: '↩', shiftEnter: '⇧↩', primaryEnter: '⌘↩' } : description,
        description,
    };
}
