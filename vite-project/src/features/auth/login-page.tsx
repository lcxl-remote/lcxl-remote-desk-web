
import { useState } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useNavigate, useSearchParams } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { Loader2, Lock, User } from "lucide-react"

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
import { useLoginAccount } from "@/services/hooks/undefinedController/useLoginAccount"
import { useGetCurrentUser } from "@/services/hooks/undefinedController/useGetCurrentUser"
import { ModeToggle } from "@/components/mode-toggle"
import { LanguageToggle } from "@/components/language-toggle"

const formSchema = z.object({
    username: z.string().min(1, {
        message: "Username is required",
    }),
    password: z.string().min(1, {
        message: "Password is required",
    }),
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

    const form = useForm<FormValues>({
        resolver: zodResolver(formSchema) as any, // Cast to any to avoid strict type mismatch issues
        defaultValues: {
            username: "",
            password: "",
            autoLogin: true,
            type: "account",
        },
    })

    async function onSubmit(values: FormValues) {
        try {
            const response = await login({
                data: {
                    username: values.username,
                    password: values.password,
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
                const redirect = searchParams.get("redirect") || "/"
                navigate(redirect)
            } else {
                toast({
                    variant: "destructive",
                    title: t("pages.login.failure", "Login failed"),
                    description: "Login failed with status: " + (response?.status || 'unknown'),
                })
            }
        } catch (error) {
            toast({
                variant: "destructive",
                title: t("pages.login.failure", "Login failed"),
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
                    <Tabs value={activeTab} onValueChange={setActiveTab} className="w-full">
                        <TabsList className="grid w-full grid-cols-2 mb-4">
                            <TabsTrigger value="account">{t('pages.login.accountLogin.tab', 'Account Login')}</TabsTrigger>
                            <TabsTrigger value="mobile" disabled>{t('pages.login.phoneLogin.tab', 'Phone Login')}</TabsTrigger>
                        </TabsList>
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
                    </Tabs>
                </CardContent>
            </Card>
            {/* Footer component placeholder */}
            <div className="fixed bottom-4 w-full text-center text-sm text-gray-500">
                Antigravity Design
            </div>
        </div>
    )
}
