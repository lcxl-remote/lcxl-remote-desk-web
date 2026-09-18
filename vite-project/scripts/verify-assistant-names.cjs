const fs = require('node:fs');
const path = require('node:path');

const oldName = new RegExp('device' + '[_ -]?assistant|co' + 'pilot|设备\\s*(?:AI\\s*)?助手', 'i');
const excluded = new Set(['.git', '.gradle', '.build', '.swiftpm', '.vite', 'cache', 'target', 'build', 'dist', 'node_modules', 'plans', 'research', 'reference-projects', 'forks', 'pocs']);
const extensions = new Set(['.rs', '.ts', '.tsx', '.js', '.cjs', '.mjs', '.kt', '.kts', '.swift', '.md', '.json', '.yaml', '.yml', '.toml', '.xml', '.xcstrings', '.sh', '.ps1', '.txt', '.html', '.sql', '.bat', '.cmd']);

// Historical archive links remain valid even when their titles predate the product name.
function currentText(text) {
    return text.replace(/(?:research|plans)\/[^\s)\]<>"'`]+/g, '');
}
function violations(root) {
    const errors = [];
    function visit(directory) {
        for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
            if (excluded.has(entry.name) || entry.isSymbolicLink()) continue;
            const file = path.join(directory, entry.name);
            if (oldName.test(entry.name)) errors.push(`${file}: obsolete file or directory name`);
            if (entry.isDirectory()) visit(file);
            else if (extensions.has(path.extname(entry.name))) {
                const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
                lines.forEach((line, index) => {
                    if (oldName.test(currentText(line))) errors.push(`${file}:${index + 1}: obsolete assistant name`);
                });
            }
        }
    }
    visit(root);
    return errors;
}
if (require.main === module) {
    const web = path.resolve(__dirname, '../..');
    const workspace = path.dirname(web);
    const roots = fs.existsSync(path.join(workspace, 'backend/manager')) ? [workspace] : [web];
    const errors = roots.flatMap(violations);
    if (errors.length) {
        process.stderr.write(errors.join('\n') + '\n');
        process.exitCode = 1;
    } else console.log('Assistant naming: current code, UI and documentation are consistent.');
}
module.exports = { oldName, currentText, violations };
