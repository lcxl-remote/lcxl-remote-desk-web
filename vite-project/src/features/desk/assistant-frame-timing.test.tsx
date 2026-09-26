import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { AssistantFrameTiming } from './assistant-frame-timing';

vi.mock('react-i18next', () => ({ useTranslation: () => ({
    t: (key: string, values?: { age: number }) => values ? `${key}:${values.age}` : key,
}) }));
afterEach(cleanup);
describe('historical screenshot timing', () => {
    it('keeps receipt age at return and discloses missing source time', () => {
        render(<AssistantFrameTiming frame={{ received_at_unix_ms: 1000, receipt_age_ms: 42, freshness: 'latest_observed' }} />);
        expect(screen.getByText('pages.aiAssistant.observation.latestObserved')).toBeTruthy();
        expect(screen.getByText(/frameAgeValue:42/)).toBeTruthy();
        expect(screen.getByText('pages.aiAssistant.observation.sourceTimeUnavailable')).toBeTruthy();
        expect(screen.getByText('pages.aiAssistant.observation.historicalFrame')).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.observation.freshFrame')).toBeNull();
    });
    it('does not infer freshness for evidence without timing', () => {
        const { container } = render(<AssistantFrameTiming frame={undefined} />);
        expect(container.textContent).toBe('');
    });
    it('does not display invalid age or claim a historical fresh frame is current', () => {
        render(<AssistantFrameTiming frame={{ received_at_unix_ms: 1000, receipt_age_ms: -1, source_timestamp_ns: 1, freshness: 'fresh' }} />);
        expect(screen.queryByText(/frameAgeValue/)).toBeNull();
        expect(screen.queryByText('pages.aiAssistant.observation.freshFrame')).toBeNull();
        expect(screen.getByText('pages.aiAssistant.observation.unavailable')).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.observation.sourceTimeUnavailable')).toBeNull();
        expect(screen.getByText('pages.aiAssistant.observation.historicalFrame')).toBeTruthy();
    });
    it.each([0, -1, Number.NaN, Number.MAX_SAFE_INTEGER + 1])('rejects invalid receipt clock %s', received => {
        render(<AssistantFrameTiming frame={{ received_at_unix_ms: received, receipt_age_ms: 0, freshness: 'fresh' }} />);
        expect(screen.queryByText('pages.aiAssistant.observation.freshFrame')).toBeNull();
        expect(screen.getByText('pages.aiAssistant.observation.unavailable')).toBeTruthy();
    });
    it('does not promote a latest observation when a producer timestamp is present', () => {
        render(<AssistantFrameTiming frame={{ received_at_unix_ms: 1000, receipt_age_ms: 20, source_timestamp_ns: 123, freshness: 'latest_observed' }} />);
        expect(screen.getByText('pages.aiAssistant.observation.latestObserved')).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.observation.freshFrame')).toBeNull();
    });

});
