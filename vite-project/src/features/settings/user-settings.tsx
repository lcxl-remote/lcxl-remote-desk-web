import { useMemo } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2, KeyRound } from "lucide-react"

import { useChangePassword } from "@/services/hooks/authController/useChangePassword"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { useToast } from "@/hooks/use-toast"
import { useGetCurrentUser } from "@/services/hooks/userController/useGetCurrentUser"

const baseSchema = z.object({
    old_password: z.string(),
    new_password: z.string(),
    confirm_password: z.string(),
})

type PasswordFormValues = z.infer<typeof baseSchema>

export function UserSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()
    const { data: userResponse } = useGetCurrentUser()
    const user = userResponse?.data

    const { mutateAsync: changePassword, isPending } = useChangePassword()

    const passwordSchema = useMemo(() => z.object({
        old_password: z.string().min(1, t("pages.user.settings.oldPasswordRequired")),
        new_password: z.string().min(8, t("pages.user.settings.newPasswordMinLength")),
        confirm_password: z.string().min(1, t("pages.user.settings.confirmPasswordRequired")),
    }).refine((data) => data.new_password === data.confirm_password, {
        message: t("pages.account.settings.passwordNotMatch"),
        path: ["confirm_password"],
    }), [t])

    const form = useForm<PasswordFormValues>({
        resolver: zodResolver(passwordSchema),
        defaultValues: {
            old_password: "",
            new_password: "",
            confirm_password: "",
        },
    })

    const onSubmit = async (values: PasswordFormValues) => {
        try {
            await changePassword({
                data: {
                    current_username: user?.name || "admin",
                    current_password: values.old_password,
                    new_password: values.new_password,
                    new_username: null,
                }
            })
            toast({
                title: t('pages.user.settings.success'),
                description: t('pages.user.settings.passwordChanged'),
            })
            form.reset()
        } catch (error: any) {
            toast({
                variant: "destructive",
                title: t('pages.user.settings.error'),
                description: error?.response?.data?.message || t('pages.user.settings.changePasswordFailed'),
            })
        }
    }

    return (
        <div className="container mx-auto max-w-2xl py-8">
            <div className="mb-8">
                <h1 className="text-3xl font-bold tracking-tight">{t('pages.user.settings.title')}</h1>
                <p className="text-muted-foreground">
                    {t('pages.user.settings.description')}
                </p>
            </div>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.user.settings.formTitle")}</CardTitle>
                    <CardDescription>{t("pages.user.settings.formDescription")}</CardDescription>
                </CardHeader>
                <CardContent>
                    <Form {...form}>
                        <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-4">
                            <FormField
                                control={form.control}
                                name="old_password"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormLabel>{t("pages.user.settings.currentPassword")}</FormLabel>
                                        <FormControl>
                                            <Input type="password" {...field} />
                                        </FormControl>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />

                            <FormField
                                control={form.control}
                                name="new_password"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormLabel>{t("pages.user.settings.newPassword")}</FormLabel>
                                        <FormControl>
                                            <Input type="password" {...field} />
                                        </FormControl>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />

                            <FormField
                                control={form.control}
                                name="confirm_password"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormLabel>{t("pages.user.settings.confirmPassword")}</FormLabel>
                                        <FormControl>
                                            <Input type="password" {...field} />
                                        </FormControl>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />

                            <div className="flex justify-end pt-4">
                                <Button type="submit" disabled={isPending}>
                                    {isPending ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <KeyRound className="mr-2 h-4 w-4" />}
                                    {t('pages.user.settings.updatePassword')}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>
        </div>
    )
}
