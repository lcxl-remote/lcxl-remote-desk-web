import { FiniteTargetBoundary } from './device-target-context';
import DeskSession from './desk-session';
export default function FiniteDeviceTarget() {
    // Anonymous OSS grants are bound to the server's private code-session cookie.
    return <div className="h-screen bg-background text-foreground"><FiniteTargetBoundary accountKey="code-session">
        <DeskSession showAssistant={false} assistantFeatureProfile={null} preferenceOwnerKey={null} />
    </FiniteTargetBoundary></div>;
}
