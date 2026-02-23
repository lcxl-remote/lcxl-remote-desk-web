import { useState } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useNavigate } from "react-router-dom"
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
import { useToast } from "@/hooks/use-toast"
import { useInitSystem } from "@/services/hooks/undefinedController/useInitSystem"
import { ModeToggle } from "@/components/mode-toggle"
import { LanguageToggle } from "@/components/language-toggle"

const formSchema = z.object({
    username: z.string().min(3, {
        message: "Username must be at least 3 characters",
    }),
    password: z.string().min(6, {
        message: "Password must be at least 6 characters",
    }),
    confirmPassword: z.string().min(1, {
        message: "Confirm password is required",
    })
}).refine((data) => data.password === data.confirmPassword, {
    message: "Passwords don't match",
    path: ["confirmPassword"],
});

type FormValues = z.infer<typeof formSchema>

export default function InitPage() {
    const { t } = useTranslation()
    const navigate = useNavigate()
    const { toast } = useToast()

    const { mutateAsync: initSystem } = useInitSystem()

    const form = useForm<FormValues>({
        resolver: zodResolver(formSchema) as any,
        defaultValues: {
            username: "",
            password: "",
            confirmPassword: "",
        },
    })

    async function onSubmit(values: FormValues) {
        try {
            await initSystem({
                data: {
                    username: values.username,
                    password: values.password,
                }
            })

            toast({
                title: t("pages.init.success", "System initialized successfully"),
            })
            navigate("/user/login")

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
            <Card className="w-[380px] sm:w-[420px] shadow-lg bg-white/90 backdrop-blur-sm dark:bg-slate-950/90">
                <CardHeader className="space-y-1 text-center">
                    <div className="flex justify-center mb-4">
                        <img alt="logo" src="/logo.svg" className="h-10 w-10" />
                    </div>
                    <CardTitle className="text-2xl font-bold">LCXL Web Remote Desk</CardTitle>
                    <CardDescription>
                        {t('pages.init.description', 'Please configure your admin account for the first time')}
                    </CardDescription>
                </CardHeader>
                <CardContent>
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
                                                <Input placeholder={t('pages.init.username.placeholder', 'Admin Username')} className="pl-9" {...field} />
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
                                                <Input type="password" placeholder={t('pages.init.password.placeholder', 'Admin Password')} className="pl-9" {...field} />
                                            </div>
                                        </FormControl>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />
                            <FormField
                                control={form.control}
                                name="confirmPassword"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormControl>
                                            <div className="relative">
                                                <Lock className="absolute left-3 top-3 h-4 w-4 text-muted-foreground" />
                                                <Input type="password" placeholder={t('pages.init.confirmPassword.placeholder', 'Confirm Password')} className="pl-9" {...field} />
                                            </div>
                                        </FormControl>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />
                            <Button type="submit" className="w-full" disabled={form.formState.isSubmitting}>
                                {form.formState.isSubmitting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                                {t('pages.init.submit', 'Initialize')}
                            </Button>
                        </form>
                    </Form>
                </CardContent>
            </Card>
            <div className="fixed bottom-4 w-full text-center text-sm text-gray-500">
                Antigravity Design
            </div>
        </div>
    )
}
