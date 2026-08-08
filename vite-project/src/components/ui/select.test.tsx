import { render, screen } from '@testing-library/react'
import '@testing-library/jest-dom'
import { afterEach, describe, expect, it } from 'vitest'

import {
    Select,
    SelectContent,
    SelectItem,
    SelectTrigger,
    SelectValue,
} from './select'

function setFullscreenElement(element: Element | null) {
    Object.defineProperty(document, 'fullscreenElement', {
        configurable: true,
        value: element,
    })
}

describe('SelectContent portal', () => {
    afterEach(() => setFullscreenElement(null))

    it('renders inside the native fullscreen subtree and above dialogs', () => {
        const fullscreen = document.createElement('div')
        document.body.appendChild(fullscreen)
        setFullscreenElement(fullscreen)

        render(
            <Select open value="x">
                <SelectTrigger>
                    <SelectValue />
                </SelectTrigger>
                <SelectContent>
                    <SelectItem value="x">X264</SelectItem>
                </SelectContent>
            </Select>,
        )

        const listbox = screen.getByRole('listbox')
        expect(fullscreen).toContainElement(listbox)
        expect(listbox).toHaveClass('z-[70]')

        fullscreen.remove()
    })
})
