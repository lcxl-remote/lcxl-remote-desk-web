import type { SVGProps } from 'react';

/** Shared assistant mark: two four-point sparkles, matching the iOS entry. */
export function AiAssistantIcon(props: SVGProps<SVGSVGElement>) {
    return (
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            width="24"
            height="24"
            fill="currentColor"
            aria-hidden="true"
            focusable="false"
            {...props}
        >
            <path d="M9 5C10.5 10.5 12.5 12.5 18 14C12.5 15.5 10.5 17.5 9 23C7.5 17.5 5.5 15.5 0 14C5.5 12.5 7.5 10.5 9 5Z" />
            <path d="M19 0C19.8 3.1 20.9 4.2 24 5C20.9 5.8 19.8 6.9 19 10C18.2 6.9 17.1 5.8 14 5C17.1 4.2 18.2 3.1 19 0Z" />
        </svg>
    );
}
