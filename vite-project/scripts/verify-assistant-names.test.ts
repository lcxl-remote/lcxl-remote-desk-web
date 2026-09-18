import { test } from 'vitest';
import assert from 'node:assert/strict';
import naming from './verify-assistant-names.cjs';
const { oldName, currentText } = naming;

test('rejects old display names and identifiers across naming conventions', () => {
    for (const separator of ['', '_', '-', ' ']) {
        assert.equal(oldName.test('Device' + separator + 'Assistant'), true);
    }
    for (const name of ['设备' + '助手', '设备 AI ' + '助手', 'co' + 'pilot', 'CO' + 'PILOT']) {
        assert.equal(oldName.test(name), true);
    }
});
test('accepts the general and terminal product names and current identifiers', () => {
    for (const name of ['AI助手', 'AI Assistant', 'Terminal AI Assistant', 'AiAssistant', 'terminal_ai_assistant', 'AI_ASSISTANT_TURN_BUSY']) {
        assert.equal(oldName.test(name), false);
    }
});
test('preserves historical archive links without exempting current prose', () => {
    const old = 'device' + '-assistant';
    assert.equal(oldName.test(currentText(`[archive](plans/2026_${old}.md)`)), false);
    assert.equal(oldName.test(currentText(`[current](docs/${old}.md)`)), true);
    assert.equal(oldName.test(currentText(('Device' + 'Assistant: plans/2026_' + old + '.md'))), true);
});
