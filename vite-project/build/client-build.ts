import { execFileSync } from 'node:child_process';

/** Read this frontend's source revision without embedding a remote URL. */
export function readClientBuild(cwd: string) {
    try {
        const number = Number(execFileSync('git', ['rev-list', '--count', 'HEAD'], { cwd, encoding: 'utf8' }).trim());
        const commit = execFileSync('git', ['rev-parse', 'HEAD'], { cwd, encoding: 'utf8' }).trim();
        return { number: Number.isSafeInteger(number) && number > 0 ? number : 0,
            commit: /^[a-f0-9]{40,64}$/.test(commit) ? commit : '' };
    } catch {
        return { number: 0, commit: '' };
    }
}
