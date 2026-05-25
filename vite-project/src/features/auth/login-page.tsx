import { useState, useEffect } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useNavigate, useSearchParams } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { Loader2, Lock, User } from "lucide-react"
import axios from "axios"

import { Button } from "@/components/ui/button"
import {
    Form,
    FormControl,
    FormField,
    FormItem,
    FormMessage,
} from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { useToast } from "@/hooks/use-toast"
import { useLoginAccount } from "@/services/hooks/authController/useLoginAccount"
import { useGetCurrentUser } from "@/services/hooks/userController/useGetCurrentUser"
import { useQueryServerInfo } from "@/services/hooks/systemController/useQueryServerInfo"
import { ModeToggle } from "@/components/mode-toggle"
import { LanguageToggle } from "@/components/language-toggle"

const formSchema = z.object({
    username: z.string().optional(),
    password: z.string().optional(),
    deviceCode: z.string().optional(),
    autoLogin: z.boolean().default(true),
    type: z.string().default("account"),
})

type FormValues = z.infer<typeof formSchema>

export default function LoginPage() {
    const { t } = useTranslation()
    const navigate = useNavigate()
    const [searchParams] = useSearchParams()
    const { toast } = useToast()
    const [activeTab, setActiveTab] = useState("account")

    const { mutateAsync: login } = useLoginAccount()
    const { refetch: fetchUserInfo } = useGetCurrentUser()
    const { data: serverInfoResp, isLoading: isServerInfoLoading } = useQueryServerInfo()

    const serverInfo = serverInfoResp?.data

    useEffect(() => {
        if (!isServerInfoLoading && serverInfo) {
            if (!serverInfo.initialized) {
                navigate("/init")
            }
        }
    }, [serverInfo, isServerInfoLoading, navigate])

    // Tauri auto-login: detect token in URL params
    useEffect(() => {
        const token = searchParams.get("token")
        if (!token) return

        const doTauriLogin = async () => {
            try {
                const response = await axios.post(`/api/login/tauri?token=${encodeURIComponent(token)}`)
                if (response.data?.status === "ok") {
                    toast({
                        title: t("pages.login.success", "Login successful"),
                    })
                    await fetchUserInfo()

                    // Navigate based on startup_mode
                    const startupMode = response.data?.startup_mode
                    if (startupMode === "desk-server" || startupMode === "desk_server") {
                        navigate("/system/settings")
                    } else {
                        navigate("/desk/list")
                    }
                    return
                }
            } catch (error) {
                // Token invalid or expired, fall through to normal login form
                console.warn("Tauri auto-login failed:", error)
            }
            // Clean up the token from URL so user sees normal login form
            const newParams = new URLSearchParams(searchParams)
            newParams.delete("token")
            window.history.replaceState({}, "", `${window.location.pathname}${newParams.toString() ? '?' + newParams.toString() : ''}`)
        }

        doTauriLogin()
    }, [searchParams]) // eslint-disable-line react-hooks/exhaustive-deps

    const form = useForm<FormValues>({
        resolver: zodResolver(formSchema) as any, // Cast to any to avoid strict type mismatch issues
        defaultValues: {
            username: "",
            password: "",
            deviceCode: "",
            autoLogin: true,
            type: "account",
        },
    })

    async function onSubmit(values: FormValues) {
        try {
            const response = await login({
                data: {
                    username: values.type === "account" ? (values.username || "") : "",
                    password: values.type === "account" ? (values.password || "") : "",
                    device_code: values.type === "device_code" ? values.deviceCode : undefined,
                    autoLogin: values.autoLogin,
                    type: values.type,
                }
            })

            // Kubb generated client returns the data directly
            if (response && response.status === 'ok') {
                toast({
                    title: t("pages.login.success", "Login successful"),
                })
                await fetchUserInfo()

                if (values.type === "device_code") {
                    const targetConnectionId = (response as any).targetConnectionId;
                    if (targetConnectionId) {
                        navigate(`/desk/${targetConnectionId}`)
                        return;
                    }
                }

                const redirect = searchParams.get("redirect") || "/"
                navigate(redirect)
            } else {
                toast({
                    variant: "destructive",
                    title: t("pages.login.failure", "Login failed"),
                    description: "Login failed with status: " + (response?.status || 'unknown'),
                })
            }
        } catch (error: any) {
            let errorMsg = error?.message || "Unknown error";
            if (error?.response?.data?.message) {
                errorMsg = error.response.data.message;
            } else if (typeof error?.response?.data === 'string') {
                errorMsg = error.response.data;
            }

            if (values.type === "device_code" && error?.response?.status === 403) {
                errorMsg = t("pages.login.deviceCode.offline", "Device is offline or device code is invalid");
            }

            toast({
                variant: "destructive",
                title: t("pages.login.failure", "Login failed"),
                description: errorMsg,
            })
        }
    }

    return (
        <div className="flex h-screen w-full items-center justify-center bg-[url('https://mdn.alipayobjects.com/yuyan_qk0oxh/afts/img/V-_oS6r-i7wAAAAAAAAAAAAAFl94AQBr')] bg-cover bg-center">
            <div className="absolute top-4 right-4 flex items-center gap-2">
                <LanguageToggle />
                <ModeToggle />
            </div>
            <Card className="w-[380px] sm:w-[420px] shadow-lg bg-white/90 backdrop-blur-sm dark:bg-slate-950/90">
                <CardHeader className="space-y-1 text-center">
                    <div className="flex justify-center mb-4">
                        <img alt="logo" src="/logo.svg" className="h-10 w-10" />
                    </div>
                    <CardTitle className="text-2xl font-bold">LCXL Web Remote Desk</CardTitle>
                    <CardDescription>
                        {t('pages.layouts.userLayout.title', 'Remote Desktop Management System')}
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    <Tabs value={activeTab} onValueChange={(val) => {
                        setActiveTab(val);
                        form.setValue("type", val);
                    }} className="w-full">
                        {serverInfo && serverInfo.startup_mode !== "desk_server" && (
                            <TabsList className="grid w-full grid-cols-2 mb-4">
                                <TabsTrigger value="account">{t('pages.login.accountLogin.tab', 'Account Login')}</TabsTrigger>
                                <TabsTrigger value="device_code">{t('pages.login.deviceCode.tab', 'Device Code Login')}</TabsTrigger>
                            </TabsList>
                        )}
                        <TabsContent value="account">
                            <Form {...form}>
                                <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-4">
                                    <FormField
                                        control={form.control}
                                        name="username"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormControl>
                                                    <div className="relative">
                                                        <User className="absolute left-3 top-3 h-4 w-4 text-muted-foreground" />
                                                        <Input placeholder={t('pages.login.username.placeholder', 'Username')} className="pl-9" {...field} />
                                                    </div>
                                                </FormControl>
                                                <FormMessage />
                                            </FormItem>
                                        )}
                                    />
                                    <FormField
                                        control={form.control}
                                        name="password"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormControl>
                                                    <div className="relative">
                                                        <Lock className="absolute left-3 top-3 h-4 w-4 text-muted-foreground" />
                                                        <Input type="password" placeholder={t('pages.login.password.placeholder', 'Password')} className="pl-9" {...field} />
                                                    </div>
                                                </FormControl>
                                                <FormMessage />
                                            </FormItem>
                                        )}
                                    />
                                    <Button type="submit" className="w-full" disabled={form.formState.isSubmitting}>
                                        {form.formState.isSubmitting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                                        {t('pages.login.submit', 'Login')}
                                    </Button>
                                </form>
                            </Form>
                        </TabsContent>
                        <TabsContent value="device_code">
                            <Form {...form}>
                                <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-4">
                                    <FormField
                                        control={form.control}
                                        name="deviceCode"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormControl>
                                                    <div className="relative">
                                                        <Lock className="absolute left-3 top-3 h-4 w-4 text-muted-foreground" />
                                                        <Input
                                                            placeholder={t('pages.login.deviceCode.placeholder', '6-digit Device Code')}
                                                            className="pl-9"
                                                            maxLength={6}
                                                            {...field}
                                                            onChange={e => {
                                                                e.target.value = e.target.value.toUpperCase();
                                                                field.onChange(e);
                                                            }}
                                                        />
                                                    </div>
                                                </FormControl>
                                                <FormMessage />
                                            </FormItem>
                                        )}
                                    />
                                    <Button type="submit" className="w-full" disabled={form.formState.isSubmitting}>
                                        {form.formState.isSubmitting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                                        {t('pages.login.submit', 'Login')}
                                    </Button>
                                </form>
                            </Form>
                        </TabsContent>
                    </Tabs>
                </CardContent>
            </Card>
            {/* Footer component placeholder */}
            <div className="fixed bottom-4 w-full text-center text-sm text-gray-500">
                LCXL Remote Desk Design
            </div>
        </div>
    )
}
