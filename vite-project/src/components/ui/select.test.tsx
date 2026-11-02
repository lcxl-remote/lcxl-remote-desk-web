import { render, screen } from '@testing-library/react'
import '@testing-library/jest-dom'
import { afterEach, describe, expect, it } from 'vitest'

import { PortalContainerContext } from './portal-container'

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

    it('uses a local stage portal while page maximization has no native fullscreen element', () => {
        setFullscreenElement(null)
        const stage = document.createElement('div')
        document.body.appendChild(stage)
        render(<PortalContainerContext.Provider value={stage}>
            <Select open value="x"><SelectTrigger><SelectValue /></SelectTrigger>
                <SelectContent><SelectItem value="x">X264</SelectItem></SelectContent>
            </Select>
        </PortalContainerContext.Provider>)
        expect(stage).toContainElement(screen.getByRole('listbox'))
        stage.remove()
    })

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
