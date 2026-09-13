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
// Radix Select uses scrolling and pointer capture; jsdom has no layout/pointers.
if (!HTMLElement.prototype.scrollIntoView) HTMLElement.prototype.scrollIntoView = () => {};
if (!HTMLElement.prototype.hasPointerCapture) HTMLElement.prototype.hasPointerCapture = () => false;
if (!HTMLElement.prototype.setPointerCapture) HTMLElement.prototype.setPointerCapture = () => {};
if (!HTMLElement.prototype.releasePointerCapture) HTMLElement.prototype.releasePointerCapture = () => {};
