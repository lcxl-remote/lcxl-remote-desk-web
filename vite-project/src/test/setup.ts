import '@testing-library/jest-dom';

// jsdom lacks ResizeObserver, which Radix UI primitives (Switch, Select, ...)
// touch on mount. Provide a no-op polyfill so component tests can render them.
if (!('ResizeObserver' in globalThis)) {
    globalThis.ResizeObserver = class ResizeObserver {
        observe() {}
        unobserve() {}
        disconnect() {}
    };
}