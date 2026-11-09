import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { expect, it } from 'vitest';
import { MarkdownContent } from './markdown-content';

const id = 'src_AAECAwQFBgcICQoLDA0ODw';
it('keeps complete but unbound and streaming markers inert without exposing partial IDs', () => {
    render(<MemoryRouter><MarkdownContent disableLinks assistantSources={[]}>{`Result 〔来源:${id}〕 and 〔来源:src_incomplete`}</MarkdownContent></MemoryRouter>);
    expect(screen.queryByRole('link')).toBeNull();
    expect(document.body.textContent).not.toContain(id);
    expect(document.body.textContent).not.toContain('src_incomplete');
});
it('opens only source markers as internal owner history links while keeping model URLs inert', () => {
    render(<MemoryRouter><MarkdownContent disableLinks assistantSources={[{source_id:id,source_revision:1}]}>{`Answer 〔来源:${id}〕. [external](https://example.com) ![image](https://example.com/pixel)`}</MarkdownContent></MemoryRouter>);
    const link = screen.getByRole('link');
    expect(link).toHaveAttribute('href', `/ai-assistant/conversation?source=${id}&revision=1`);
    expect(link).toHaveAttribute('target', '_blank');
    expect(link).toHaveAttribute('rel', 'noopener noreferrer');
    expect(screen.getByText('external').tagName).toBe('SPAN');
    expect(document.querySelector('img')).toBeNull();
});

it('does not activate code, existing link labels, malformed markers or other Markdown surfaces', () => {
    const view = render(<MemoryRouter><MarkdownContent disableLinks assistantSources={[{source_id:id,source_revision:1}]}>{`\`〔来源:${id}〕\` [〔来源:${id}〕](https://example.com) 〔来源:src_fake〕\n\n\`\`\`\n〔来源:${id}〕\n\`\`\``}</MarkdownContent></MemoryRouter>);
    expect(screen.queryByRole('link')).toBeNull();
    view.rerender(<MemoryRouter><MarkdownContent disableLinks>{`〔来源:${id}〕`}</MarkdownContent></MemoryRouter>);
    expect(screen.queryByRole('link')).toBeNull();
});

it('resolves conversation alias markers through the citation list to native source links', () => {
    render(<MemoryRouter><MarkdownContent disableLinks assistantSources={[{source_id:id,source_revision:2,alias:'src3'}]}>{`Answer 〔来源:src3〕 and 〔来源:src4〕`}</MarkdownContent></MemoryRouter>);
    const link = screen.getByRole('link');
    expect(link).toHaveAttribute('href', `/ai-assistant/conversation?source=${id}&revision=2`);
    expect(document.body.textContent).not.toContain('src3');
    expect(document.body.textContent).not.toContain('src4');
});
