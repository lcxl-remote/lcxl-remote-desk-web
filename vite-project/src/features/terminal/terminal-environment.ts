import type { OperationSystemEnum } from '@/services/types';

/** Extract the executable name from the comma-delimited terminal command. */
export function terminalShell(command: string): string {
    return (command.split(',')[0] || '').split(/[\\/]/).pop() || command;
}

/**
 * Translate the host-reported OS into the model-facing terminal label.
 *
 * A shell name is intentionally not used as a fallback: bash, zsh, fish and
 * pwsh are all available on more than one OS. When the connection handshake
 * did not report a platform, `unknown` is safer than inventing Linux.
 */
export function terminalOs(operationSystem: OperationSystemEnum | undefined): string {
    switch (operationSystem) {
        case 'Windows':
            return 'windows';
        case 'Linux':
            return 'linux';
        case 'Mac':
            return 'macos';
        case 'Android':
            return 'android';
        case 'Ios':
            return 'ios';
        case 'Web':
            return 'web';
        default:
            return 'unknown';
    }
}
