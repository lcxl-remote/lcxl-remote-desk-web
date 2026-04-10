# Frontend Settings Page Optimization

## Objective
Convert the current frontend Settings layout (which was a flattened structure in the main app sidebar or a split-screen design) into a clean, "Control Panel" Overview. Users click "Settings" on the main sidebar, navigating to a centralized grid of cards grouped by categories ("General", "Signal", "Desk"). Selecting a card brings them to specific setting pages, which occupy the full width of the screen, and provides a "Back to Settings Overview" button to return to the grid.

## Implementation Plan

1. **i18n Preparation**:
   - Added translation keys to `pages.ts` (both `zh-CN` and `en-US`) for the new setting categories and actions:
     - `pages.settings.category.general` -> `通用设置` / `General Settings`
     - `pages.settings.category.signal` -> `Signal 服务端设置` / `Signal Server Settings`
     - `pages.settings.category.desk` -> `Desk 服务端设置` / `Desk Server Settings`
     - `pages.settings.backToOverview` -> `返回设置概览` / `Back to Settings Overview`

2. **Create `SettingsOverview` Component**:
   - Created a new component `vite-project/src/features/settings/settings-overview.tsx`.
   - Utilized `Card`, `CardHeader`, `CardTitle`, `CardDescription` from shadcn/ui.
   - Read `serverInfo.startup_mode` to conditionally render "Signal" and "Desk" setting categories.
   - Rendered responsive grids of cards (`grid-cols-1 md:grid-cols-2 lg:grid-cols-3`) for each setting module.
   - Applied appropriate `lucide-react` icons (e.g., `Settings`, `FileText`, `Server`, `Key`, `Shield`, `Network`) to provide visual hierarchy for the cards.

3. **Update `SettingsLayout` Component**:
   - Created the wrapper layout `vite-project/src/features/settings/settings-layout.tsx`.
   - Removed the previous secondary sidebar/split-pane layout.
   - Added a top navigation bar that conditionally displays a "<- Back to Settings Overview" button (using `ArrowLeft` from `lucide-react`) when not on the root overview path (`/system`).
   - The content specific to each setting renders within an `<Outlet />` occupying the full width.

4. **Refactor Routes (`router.tsx`)**:
   - Updated `vite-project/src/app/router.tsx` by replacing the flattened `/system/*` routes with a nested structure.
   - Bound the base route `path: 'system'` to `<SettingsLayout />`.
   - Configured `index: true` inside `/system` to render the newly created `<SettingsOverview />` component instead of redirecting.

5. **Simplify Main Sidebar (`app-sidebar.tsx`)**:
   - Replaced the large collapsed settings group in `vite-project/src/features/layout/app-sidebar.tsx` with a single entry pointing to `/system` (labeled as "Settings").

## Task List

- [x] Update i18n JSON/TS maps (`pages.ts`) for categorizing labels and the back button string.
- [x] Create `SettingsOverview.tsx` mapping available settings to UI cards.
- [x] Create `SettingsLayout.tsx` for the shared settings view layout that handles backward navigation to the overview.
- [x] Integrate `SettingsLayout` and `SettingsOverview` into the main `router.tsx` route tree under `/system`.
- [x] Remove the bloated multi-item settings list from `app-sidebar.tsx` and point the solitary "Settings" menu node to `/system`.
- [x] Verify the build succeeds with TypeScript `npx tsc --noEmit`.

## Walkthrough / Execution Summary

The optimization successfully resolves UI clutter. Initially, attempting a split layout (sidebar inside settings) still consumed too much horizontal space for individual setting configurations. By shifting to a "Control Panel" design (Overview cards that lead to full-page setting modules with a back button), the main application maintains a neat, single "Settings" button in its navigation. 

When entering `/system`, users are presented with a clear dashboard representing their server capabilities dynamically driven by `serverInfo.startup_mode`. For instance, Desk-related tools naturally hide if the user operates merely in a Signal-only node. Returning from deep configuration screens (like `System Settings`) happens through a convenient back button injected cleanly above the view by the `SettingsLayout` container.
