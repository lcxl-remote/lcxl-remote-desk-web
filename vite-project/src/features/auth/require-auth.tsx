
import { Navigate, useLocation } from "react-router-dom";
import { Loader2 } from "lucide-react";
import { useGetCurrentUser } from "@/services/hooks/undefinedController/useGetCurrentUser";

export default function RequireAuth({ children }: { children: React.ReactNode }) {
    const location = useLocation();
    const { data: user, isLoading, isError } = useGetCurrentUser({
        query: {
            retry: false,
            staleTime: 5 * 60 * 1000,
        }
    });

    if (isLoading) {
        return (
            <div className="flex h-screen w-full items-center justify-center">
                <Loader2 className="h-8 w-8 animate-spin" />
            </div>
        );
    }

    if (isError || !user) {
        return <Navigate to="/user/login" state={{ from: location }} replace />;
    }

    return children;
}
