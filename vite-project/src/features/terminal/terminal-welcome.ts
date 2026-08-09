const WIDE_BANNER_MIN_COLUMNS = 76;

const WIDE_BANNER = [
    " _      ____  __  __ _",
    "| |    / ___| \\ \\/ /| |",
    "| |   | |      \\  / | |",
    "| |___| |___   /  \\ | |___",
    "|_____|\\____| /_/\\_\\|_____|",
    "",
    " ____                      _         ____            _",
    "|  _ \\ ___ _ __ ___   ___ | |_ ___ |  _ \\  ___  ___| | __",
    "| |_) / _ \\ '_ ` _ \\ / _ \\| __/ _ \\| | | |/ _ \\/ __| |/ /",
    "|  _ <  __/ | | | | | (_) | ||  __/| |_| |  __/\\__ \\   <",
    "|_| \\_\\___|_| |_| |_|\\___/ \\__\\___||____/ \\___||___/_|\\_\\",
];

const COMPACT_BANNER = [
    "+------------------+",
    "| LCXL Remote Desk |",
    "+------------------+",
];

export function terminalWelcomeBanner(columns: number): string {
    const lines = columns >= WIDE_BANNER_MIN_COLUMNS ? WIDE_BANNER : COMPACT_BANNER;
    return `\x1b[36m${lines.join("\r\n")}\x1b[0m\r\n\r\n`;
}
