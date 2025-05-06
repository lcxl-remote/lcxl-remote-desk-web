export function formatSize(size?: number): string {
    if (size === undefined || size === null) {
        return "0 B"; // Return a default value or handle the undefined case as needed
    }
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    let i = 0;
    while (size >= 1024 && i < units.length - 1) {
        size /= 1024;
        i++;
    }
    return `${size.toFixed(2)} ${units[i]}`;
}