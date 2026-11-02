import { createContext, useContext } from 'react';
import { fullscreenPortalContainer } from '@/lib/utils';

export const PortalContainerContext = createContext<HTMLElement | undefined>(undefined);
export function usePortalContainer() {
    return useContext(PortalContainerContext) ?? fullscreenPortalContainer();
}
