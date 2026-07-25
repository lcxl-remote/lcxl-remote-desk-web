import { useEffect, useRef } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2, Save } from "lucide-react"

import { useQueryAiPolicySettings } from "@/services/hooks/aiModelController/useQueryAiPolicySettings"
import { useUpdateAiPolicySettings } from "@/services/hooks/aiModelController/useUpdateAiPolicySettings"
import { useQueryCollectionPolicySettings } from "@/services/hooks/aiModelController/useQueryCollectionPolicySettings"
import { useUpdateCollectionPolicySettings } from "@/services/hooks/aiModelController/useUpdateCollectionPolicySettings"
import type { AiExecutionPolicyUpdate, CollectionPolicySettingsUpdate } from "@/services/types"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { useToast } from "@/hooks/use-toast"

const EXECUTION_MODES = ["suggest_only", "read_only", "confirm_each_action"] as const
const MIN_CONCURRENT_EXECUTIONS = 1
const MAX_CONCURRENT_EXECUTIONS = 64

const policySchema = z.object({
    execution_mode: z.enum(EXECUTION_MODES),
    max_concurrent_executions: z
        .number()
        .int()
        .min(MIN_CONCURRENT_EXECUTIONS)
        .max(MAX_CONCURRENT_EXECUTIONS),
})

type PolicyFormValues = z.infer<typeof policySchema>

const collectionPolicySchema = z.object({
    allow_screen: z.boolean(),
    allow_logs: z.boolean(),
})

type CollectionPolicyFormValues = z.infer<typeof collectionPolicySchema>

function normalizeExecutionMode(mode: string | undefined): (typeof EXECUTION_MODES)[number] {
    return EXECUTION_MODES.includes(mode as (typeof EXECUTION_MODES)[number])
        ? (mode as (typeof EXECUTION_MODES)[number])
        : "suggest_only"
}

function ExecutionModeItems() {
    const { t } = useTranslation()
    return (
        <>
            <SelectItem value="suggest_only">{t("pages.executionMode.suggestOnly")}</SelectItem>
            <SelectItem value="read_only">{t("pages.executionMode.readOnly")}</SelectItem>
            <SelectItem value="confirm_each_action">{t("pages.executionMode.confirmEachAction")}</SelectItem>
        </>
    )
}

export function AiPolicySettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    // Both policies are enforced by the Desk Server on this device. The central
    // Signal grant can only narrow the resulting permissions, never widen them.
    const { data: policyResponse, isLoading: isPolicyLoading } = useQueryAiPolicySettings()
    const { mutateAsync: updatePolicy, isPending: isPolicyUpdating } = useUpdateAiPolicySettings()
    const { data: collectionResponse, isLoading: isCollectionLoading } = useQueryCollectionPolicySettings()
    const { mutateAsync: updateCollection, isPending: isCollectionUpdating } = useUpdateCollectionPolicySettings()

    const policyForm = useForm<PolicyFormValues>({
        resolver: zodResolver(policySchema),
        defaultValues: { execution_mode: "suggest_only", max_concurrent_executions: 4 },
    })
    const collectionForm = useForm<CollectionPolicyFormValues>({
        resolver: zodResolver(collectionPolicySchema),
        defaultValues: { allow_screen: false, allow_logs: false },
    })

    const didHydratePolicyRef = useRef(false)
    useEffect(() => {
        if (policyResponse?.data && !isPolicyLoading && !didHydratePolicyRef.current) {
            didHydratePolicyRef.current = true
            policyForm.reset({
                execution_mode: normalizeExecutionMode(policyResponse.data.execution_mode),
                max_concurrent_executions:
                    policyResponse.data.max_concurrent_executions ?? 4,
            })
        }
    }, [policyResponse?.data, isPolicyLoading, policyForm])

    const didHydrateCollectionRef = useRef(false)
    useEffect(() => {
        if (collectionResponse?.data && !isCollectionLoading && !didHydrateCollectionRef.current) {
            didHydrateCollectionRef.current = true
            collectionForm.reset({
                allow_screen: collectionResponse.data.allow_screen ?? false,
                allow_logs: collectionResponse.data.allow_logs ?? false,
            })
        }
    }, [collectionResponse?.data, isCollectionLoading, collectionForm])

    const onSubmitPolicy = async (values: PolicyFormValues) => {
        const payload: AiExecutionPolicyUpdate = {
            execution_mode: values.execution_mode,
            max_concurrent_executions: values.max_concurrent_executions,
        }
        try {
            await updatePolicy({ data: payload })
            toast({
                title: t("pages.system.settings.success"),
                description: t("pages.aiPolicy.updateSucceedMessage"),
            })
        } catch {
            toast({
                variant: "destructive",
                title: t("pages.system.settings.error"),
                description: t("pages.aiPolicy.updateFailedMessage"),
            })
        }
    }

    const onSubmitCollection = async (values: CollectionPolicyFormValues) => {
        const payload: CollectionPolicySettingsUpdate = {
            allow_screen: values.allow_screen,
            allow_logs: values.allow_logs,
        }
        try {
            await updateCollection({ data: payload })
            toast({
                title: t("pages.system.settings.success"),
                description: t("pages.collectionPolicy.updateSucceedMessage"),
            })
        } catch {
            toast({
                variant: "destructive",
                title: t("pages.system.settings.error"),
                description: t("pages.collectionPolicy.updateFailedMessage"),
            })
        }
    }

    if (isPolicyLoading || isCollectionLoading) {
        return (
            <div className="flex h-full items-center justify-center">
                <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
            </div>
        )
    }

    return (
        <div className="container mx-auto max-w-4xl py-8">
            <div className="mb-8">
                <h1 className="text-3xl font-bold tracking-tight">{t("pages.aiPolicy.settings.title")}</h1>
                <p className="text-muted-foreground">
                    {t("pages.aiPolicy.settings.description")}
                </p>
            </div>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.aiPolicy.title")}</CardTitle>
                    <CardDescription>{t("pages.aiPolicy.description")}</CardDescription>
                </CardHeader>
                <CardContent>
                    <Form {...policyForm}>
                        <form onSubmit={policyForm.handleSubmit(onSubmitPolicy)} className="space-y-4">
                            <FormField
                                control={policyForm.control}
                                name="execution_mode"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormLabel>{t("pages.aiPolicy.executionMode")}</FormLabel>
                                        <Select
                                            key={field.value || "ceiling-empty"}
                                            onValueChange={field.onChange}
                                            defaultValue={field.value}
                                        >
                                            <FormControl>
                                                <SelectTrigger>
                                                    <SelectValue />
                                                </SelectTrigger>
                                            </FormControl>
                                            <SelectContent>
                                                <ExecutionModeItems />
                                            </SelectContent>
                                        </Select>
                                        <FormDescription>
                                            {t("pages.aiPolicy.executionMode.description")}
                                        </FormDescription>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />
                            <FormField
                                control={policyForm.control}
                                name="max_concurrent_executions"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormLabel>{t("pages.aiPolicy.maxConcurrentExecutions")}</FormLabel>
                                        <FormControl>
                                            <Input
                                                type="number"
                                                min={MIN_CONCURRENT_EXECUTIONS}
                                                max={MAX_CONCURRENT_EXECUTIONS}
                                                name={field.name}
                                                ref={field.ref}
                                                onBlur={field.onBlur}
                                                value={field.value}
                                                onChange={(e) =>
                                                    field.onChange(
                                                        e.target.value === ""
                                                            ? Number.NaN
                                                            : e.target.valueAsNumber,
                                                    )
                                                }
                                            />
                                        </FormControl>
                                        <FormDescription>
                                            {t("pages.aiPolicy.maxConcurrentExecutions.description")}
                                        </FormDescription>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />
                            <div className="flex justify-end">
                                <Button type="submit" disabled={isPolicyUpdating}>
                                    {isPolicyUpdating ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Save className="mr-2 h-4 w-4" />}
                                    {t("pages.system.settings.save")}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>

            <Card className="mt-6">
                <CardHeader>
                    <CardTitle>{t("pages.collectionPolicy.title")}</CardTitle>
                    <CardDescription>{t("pages.collectionPolicy.description")}</CardDescription>
                </CardHeader>
                <CardContent>
                    <Form {...collectionForm}>
                        <form onSubmit={collectionForm.handleSubmit(onSubmitCollection)} className="space-y-4">
                            <FormField
                                control={collectionForm.control}
                                name="allow_logs"
                                render={({ field }) => (
                                    <FormItem className="flex flex-row items-center justify-between rounded-lg border p-3 shadow-sm">
                                        <div className="space-y-0.5">
                                            <FormLabel>{t("pages.collectionPolicy.allowLogs")}</FormLabel>
                                            <FormDescription>
                                                {t("pages.collectionPolicy.allowLogs.description")}
                                            </FormDescription>
                                        </div>
                                        <FormControl>
                                            <Switch checked={field.value} onCheckedChange={field.onChange} />
                                        </FormControl>
                                    </FormItem>
                                )}
                            />
                            <FormField
                                control={collectionForm.control}
                                name="allow_screen"
                                render={({ field }) => (
                                    <FormItem className="flex flex-row items-center justify-between rounded-lg border p-3 shadow-sm">
                                        <div className="space-y-0.5">
                                            <FormLabel>{t("pages.collectionPolicy.allowScreen")}</FormLabel>
                                            <FormDescription>
                                                {t("pages.collectionPolicy.allowScreen.description")}
                                            </FormDescription>
                                        </div>
                                        <FormControl>
                                            <Switch checked={field.value} onCheckedChange={field.onChange} />
                                        </FormControl>
                                    </FormItem>
                                )}
                            />
                            <div className="flex justify-end">
                                <Button type="submit" disabled={isCollectionUpdating}>
                                    {isCollectionUpdating ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Save className="mr-2 h-4 w-4" />}
                                    {t("pages.system.settings.save")}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>
        </div>
    )
}
