import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useNavigate } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { Loader2, Lock, User } from "lucide-react"
import { useQueryClient } from "@tanstack/react-query"
import { queryServerInfoQueryKey } from "@/services/hooks/undefinedController/useQueryServerInfo"

import { Button } from "@/components/ui/button"
import {
    Form,
    FormControl,
    FormField,
    FormItem,
    FormLabel,
    FormMessage,
    FormDescription,
} from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Checkbox } from "@/components/ui/checkbox"
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card"
import { useToast } from "@/hooks/use-toast"
import { useInitSystem } from "@/services/hooks/undefinedController/useInitSystem"
import { ModeToggle } from "@/components/mode-toggle"
import { LanguageToggle } from "@/components/language-toggle"
import { TelemetryDisclosure } from "@/components/telemetry-disclosure"

export default function InitPage() {
    const { t } = useTranslation()
    const navigate = useNavigate()
    const { toast } = useToast()
    const queryClient = useQueryClient()

    const formSchema = z.object({
        username: z.string().min(3, {
            message: t("pages.init.username.min", "Username must be at least 3 characters"),
        }),
        password: z.string().min(6, {
            message: t("pages.init.password.min", "Password must be at least 6 characters"),
        }),
        confirmPassword: z.string().min(1, {
            message: t("pages.init.confirmPassword.required", "Confirm password is required"),
        }),
        telemetryConsent: z.boolean().default(true),
    }).refine((data) => data.password === data.confirmPassword, {
        message: t("pages.init.confirmPassword.match", "Passwords don't match"),
        path: ["confirmPassword"],
    });

    type FormValues = z.infer<typeof formSchema>

    const { mutateAsync: initSystem } = useInitSystem()

    const form = useForm<FormValues>({
        resolver: zodResolver(formSchema) as any,
        defaultValues: {
            username: "",
            password: "",
            confirmPassword: "",
            telemetryConsent: true,
        },
    })

    async function onSubmit(values: FormValues) {
        try {
            await initSystem({
                data: {
                    username: values.username,
                    password: values.password,
                    telemetry_consent: values.telemetryConsent,
                }
            })

            // Remove server info query to ensure other pages (like Login) 
            // must fetch the fresh "initialized: true" state.
            // Using removeQueries instead of invalidateQueries to avoid 
            // the case where a new page sees stale cached data before the refetch finishes.
            queryClient.removeQueries({ queryKey: queryServerInfoQueryKey() })

            toast({
                title: t("pages.init.success", "System initialized successfully"),
            })
            navigate("/")

        } catch (error) {
            toast({
                variant: "destructive",
                title: t("pages.init.failure", "System initialization failed"),
                description: (error as Error).message,
            })
        }
    }

    return (
        <div className="flex h-screen w-full items-center justify-center bg-[url('https://mdn.alipayobjects.com/yuyan_qk0oxh/afts/img/V-_oS6r-i7wAAAAAAAAAAAAAFl94AQBr')] bg-cover bg-center">
            <div className="absolute top-4 right-4 flex items-center gap-2">
                <LanguageToggle />
                <ModeToggle />
            </div>

            <div className="flex flex-col items-center animate-in fade-in slide-in-from-bottom-8 duration-1000 px-4">
                <div className="text-center mb-8 space-y-2">
                    <div className="flex justify-center mb-4">
                        <div className="p-3 bg-white/20 backdrop-blur-md rounded-2xl shadow-xl border border-white/30">
                            <img alt="logo" src="/logo.svg" className="h-12 w-12" />
                        </div>
                    </div>
                    <h1 className="text-3xl font-extrabold tracking-tight text-slate-900 dark:text-white drop-shadow-sm sm:text-5xl">
                        {t('pages.init.welcome', 'Welcome to LCXL Remote Desk')}
                    </h1>
                    <p className="text-lg text-slate-600 dark:text-white/80 font-medium">
                        {t('pages.init.subWelcome', 'Secure, fast, and cross-platform remote desktop solution')}
                    </p>
                </div>

                <Card className="w-full max-w-[450px] shadow-2xl bg-white/95 backdrop-blur-md dark:bg-slate-950/95 border-none">
                    <CardHeader className="space-y-1 text-center pb-2">
                        <CardTitle className="text-2xl font-bold text-primary">
                            {t('pages.init.title', 'System Initialization')}
                        </CardTitle>
                        <CardDescription className="text-sm">
                            {t('pages.init.description', 'Please configure your admin account for the first time')}
                        </CardDescription>
                    </CardHeader>
                    <CardContent className="pt-4">
                        <Form {...form}>
                            <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-5">
                                <div className="space-y-4">
                                    <FormField
                                        control={form.control}
                                        name="username"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormControl>
                                                    <div className="relative group">
                                                        <User className="absolute left-3 top-3 h-4 w-4 text-muted-foreground group-focus-within:text-primary transition-colors" />
                                                        <Input placeholder={t('pages.init.username.placeholder', 'Admin Username')} className="pl-9 h-11 bg-slate-50/50 dark:bg-slate-900/50 border-slate-200 dark:border-slate-800 transition-all focus:ring-2 focus:ring-primary/20" {...field} />
                                                    </div>
                                                </FormControl>
                                                <FormMessage className="text-xs" />
                                            </FormItem>
                                        )}
                                    />
                                    <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                                        <FormField
                                            control={form.control}
                                            name="password"
                                            render={({ field }) => (
                                                <FormItem>
                                                    <FormControl>
                                                        <div className="relative group">
                                                            <Lock className="absolute left-3 top-3 h-4 w-4 text-muted-foreground group-focus-within:text-primary transition-colors" />
                                                            <Input type="password" placeholder={t('pages.init.password.placeholder', 'Admin Password')} className="pl-9 h-11 bg-slate-50/50 dark:bg-slate-900/50 border-slate-200 dark:border-slate-800 transition-all focus:ring-2 focus:ring-primary/20" {...field} />
                                                        </div>
                                                    </FormControl>
                                                    <FormMessage className="text-xs" />
                                                </FormItem>
                                            )}
                                        />
                                        <FormField
                                            control={form.control}
                                            name="confirmPassword"
                                            render={({ field }) => (
                                                <FormItem>
                                                    <FormControl>
                                                        <div className="relative group">
                                                            <Lock className="absolute left-3 top-3 h-4 w-4 text-muted-foreground group-focus-within:text-primary transition-colors" />
                                                            <Input type="password" placeholder={t('pages.init.confirmPassword.placeholder', 'Confirm Password')} className="pl-9 h-11 bg-slate-50/50 dark:bg-slate-900/50 border-slate-200 dark:border-slate-800 transition-all focus:ring-2 focus:ring-primary/20" {...field} />
                                                        </div>
                                                    </FormControl>
                                                    <FormMessage className="text-xs" />
                                                </FormItem>
                                            )}
                                        />
                                    </div>
                                </div>

                                <FormField
                                    control={form.control}
                                    name="telemetryConsent"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-start space-x-3 space-y-0 rounded-xl border border-slate-200 dark:border-slate-800 p-4 bg-slate-50/30 dark:bg-slate-900/30 hover:bg-slate-50/50 dark:hover:bg-slate-900/50 transition-colors">
                                            <FormControl>
                                                <Checkbox
                                                    checked={field.value}
                                                    onCheckedChange={field.onChange}
                                                    className="mt-1"
                                                />
                                            </FormControl>
                                            <div className="space-y-1 leading-none">
                                                <FormLabel className="text-sm font-semibold cursor-pointer">
                                                    {t('pages.init.telemetry.label', 'Enable Telemetry')}
                                                </FormLabel>
                                                <div className="flex items-center gap-2">
                                                    <FormDescription className="text-xs leading-relaxed">
                                                        {t('pages.init.telemetry.description', 'Send anonymous usage data to help improve this product')}
                                                    </FormDescription>
                                                    <TelemetryDisclosure />
                                                </div>
                                            </div>
                                        </FormItem>
                                    )}
                                />

                                <Button type="submit" className="w-full h-11 text-base font-semibold shadow-lg hover:shadow-xl transition-all shadow-primary/20" disabled={form.formState.isSubmitting}>
                                    {form.formState.isSubmitting ? (
                                        <>
                                            <Loader2 className="mr-2 h-5 w-5 animate-spin" />
                                            {t('pages.init.submitting', 'Initializing...')}
                                        </>
                                    ) : (
                                        t('pages.init.submit', 'Initialize System')
                                    )}
                                </Button>
                            </form>
                        </Form>
                    </CardContent>
                </Card>
            </div>

            <div className="fixed bottom-6 w-full text-center text-sm font-medium text-white/60 drop-shadow-md">
                © {new Date().getFullYear()} LCXL Team. Built with Passion.
            </div>
        </div>
    )
}
