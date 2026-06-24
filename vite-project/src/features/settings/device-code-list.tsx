import { useState } from "react"
import { useTranslation } from "react-i18next"
import { Loader2, Plus, RefreshCw, Trash2, Edit2 } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { useToast } from "@/hooks/use-toast"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "@/components/ui/dialog"

import { useListDeviceCodes } from "@/services/hooks/deviceCodeController/useListDeviceCodes"
import { useCreateDeviceCode } from "@/services/hooks/deviceCodeController/useCreateDeviceCode"
import { useUpdateDeviceCode } from "@/services/hooks/deviceCodeController/useUpdateDeviceCode"
import { useDeleteDeviceCode } from "@/services/hooks/deviceCodeController/useDeleteDeviceCode"
import { useBatchDeleteDeviceCodes } from "@/services/hooks/deviceCodeController/useBatchDeleteDeviceCodes"
import type { DeviceCodeItem } from "@/services/types"
import { Badge } from "@/components/ui/badge"
import { Checkbox } from "@/components/ui/checkbox"
import { v4 as uuidv4 } from "uuid"

export function DeviceCodeList() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const [page, setPage] = useState(1)
    const pageSize = 20

    const { data: listResp, isLoading, refetch } = useListDeviceCodes({
        page: page as any,
        page_size: pageSize as any
    })

    const { mutateAsync: createDeviceCode, isPending: isCreating } = useCreateDeviceCode()
    const { mutateAsync: updateDeviceCode } = useUpdateDeviceCode()
    const { mutateAsync: deleteDeviceCode } = useDeleteDeviceCode()
    const { mutateAsync: batchDeleteDeviceCodes, isPending: isBatchDeleting } = useBatchDeleteDeviceCodes()

    const list = listResp?.data?.items || []
    const [selectedIds, setSelectedIds] = useState<number[]>([])

    const [isCreateOpen, setIsCreateOpen] = useState(false)
    const [createForm, setCreateForm] = useState({ clientId: '', deviceCode: '' })

    const [editItem, setEditItem] = useState<DeviceCodeItem | null>(null)
    const [editForm, setEditForm] = useState({ deviceCode: '' })

    const handleOpenCreate = () => {
        const client_id = uuidv4()
        const charset = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ"
        let device_code = ""
        for (let i = 0; i < 6; i++) {
            device_code += charset.charAt(Math.floor(Math.random() * charset.length))
        }
        setCreateForm({ clientId: client_id, deviceCode: device_code })
        setIsCreateOpen(true)
    }

    const submitCreate = async () => {
        if (!createForm.clientId || !createForm.deviceCode) return;
        try {
            await createDeviceCode({
                data: {
                    client_id: createForm.clientId,
                    device_code: createForm.deviceCode
                }
            })
            toast({
                title: t('pages.deviceCodeList.generateSuccess')
            })
            setIsCreateOpen(false)
            refetch()
        } catch (error) {
            toast({
                variant: 'destructive',
                title: t('pages.deviceCodeList.generateFailed'),
                description: (error as Error).message
            })
        }
    }

    const handleDelete = async (id: number) => {
        if (!confirm(t('pages.deviceCodeList.deleteConfirm.description'))) return

        try {
            await deleteDeviceCode({ id })
            toast({
                title: t('pages.deviceCodeList.deleteSuccess')
            })
            setSelectedIds(prev => prev.filter(i => i !== id))
            refetch()
        } catch (error) {
            toast({
                variant: 'destructive',
                title: t('pages.deviceCodeList.deleteFailed'),
                description: (error as Error).message
            })
        }
    }

    const handleOpenEdit = (item: DeviceCodeItem) => {
        setEditItem(item)
        setEditForm({ deviceCode: item.deviceCode })
    }

    const submitEdit = async () => {
        if (!editItem || !editForm.deviceCode) return;
        if (editForm.deviceCode === editItem.deviceCode) {
            setEditItem(null)
            return
        }

        try {
            await updateDeviceCode({
                id: editItem.id,
                data: {
                    device_code: editForm.deviceCode
                }
            })
            toast({
                title: t('pages.deviceCodeList.updateSuccess')
            })
            setEditItem(null)
            refetch()
        } catch (error) {
            toast({
                variant: 'destructive',
                title: t('pages.deviceCodeList.updateFailed'),
                description: (error as Error).message
            })
        }
    }

    const handleBatchDelete = async () => {
        if (!selectedIds.length) return;
        if (!confirm(t('pages.deviceCodeList.batchDeleteConfirm'))) return;

        try {
            await batchDeleteDeviceCodes({ data: { ids: selectedIds } })
            toast({
                title: t('pages.deviceCodeList.deleteSuccess')
            })
            setSelectedIds([])
            refetch()
        } catch (error) {
            toast({
                variant: 'destructive',
                title: t('pages.deviceCodeList.deleteFailed'),
                description: (error as Error).message
            })
        }
    }

    return (
        <div className="container mx-auto max-w-5xl py-8">
            <div className="mb-8 flex items-center justify-between">
                <div>
                    <h1 className="text-3xl font-bold tracking-tight">{t('pages.deviceCodeList.title')}</h1>
                    <p className="text-muted-foreground">
                        {t('pages.deviceCodeList.description')}
                    </p>
                </div>
                <div className="flex gap-2 items-center">
                    {selectedIds.length > 0 && (
                        <Button variant="destructive" onClick={handleBatchDelete} disabled={isBatchDeleting}>
                            {isBatchDeleting ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Trash2 className="mr-2 h-4 w-4" />}
                            {t('pages.deviceCodeList.batchDelete')} ({selectedIds.length})
                        </Button>
                    )}
                    <Button variant="outline" onClick={() => refetch()}>
                        <RefreshCw className="mr-2 h-4 w-4" />
                        {t('common.refresh')}
                    </Button>
                    <Button onClick={handleOpenCreate} disabled={isCreating}>
                        {isCreating ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Plus className="mr-2 h-4 w-4" />}
                        {t('pages.deviceCodeList.generate')}
                    </Button>
                </div>
            </div>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.deviceCodeList.title")}</CardTitle>
                </CardHeader>
                <CardContent>
                    {isLoading ? (
                        <div className="flex justify-center py-8">
                            <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
                        </div>
                    ) : (
                        <div className="rounded-md border">
                            <Table>
                                <TableHeader>
                                    <TableRow>
                                        <TableHead className="w-[50px]">
                                            <Checkbox
                                                checked={list.length > 0 && selectedIds.length === list.filter((c: DeviceCodeItem) => !c.isOnline).length}
                                                onCheckedChange={(checked) => {
                                                    if (checked) {
                                                        setSelectedIds(list.filter((c: DeviceCodeItem) => !c.isOnline).map((c: DeviceCodeItem) => c.id))
                                                    } else {
                                                        setSelectedIds([])
                                                    }
                                                }}
                                            />
                                        </TableHead>
                                        <TableHead>ID</TableHead>
                                        <TableHead>{t('pages.deviceCodeList.column.clientId')}</TableHead>
                                        <TableHead>{t('pages.deviceCodeList.column.deviceCode')}</TableHead>
                                        <TableHead>{t('pages.deviceCodeList.column.createdAt')}</TableHead>
                                        <TableHead>{t('pages.deviceCodeList.column.updatedAt')}</TableHead>
                                        <TableHead className="text-right">{t('common.actions')}</TableHead>
                                    </TableRow>
                                </TableHeader>
                                <TableBody>
                                    {list.length === 0 ? (
                                        <TableRow>
                                            <TableCell colSpan={6} className="h-24 text-center">
                                                No results.
                                            </TableCell>
                                        </TableRow>
                                    ) : (
                                        list.map((item: DeviceCodeItem) => (
                                            <TableRow key={item.id}>
                                                <TableCell>
                                                    <Checkbox
                                                        checked={selectedIds.includes(item.id)}
                                                        disabled={item.isOnline}
                                                        onCheckedChange={(checked) => {
                                                            if (checked) {
                                                                setSelectedIds(prev => [...prev, item.id])
                                                            } else {
                                                                setSelectedIds(prev => prev.filter(i => i !== item.id))
                                                            }
                                                        }}
                                                    />
                                                </TableCell>
                                                <TableCell>{item.id}</TableCell>
                                                <TableCell className="font-mono">{item.clientId}</TableCell>
                                                <TableCell className="font-mono">
                                                    {item.deviceCode}
                                                    {item.isOnline ? (
                                                        <Badge variant="default" className="ml-2 bg-green-500 hover:bg-green-600">
                                                            {t('pages.deviceCodeList.status.online')}
                                                        </Badge>
                                                    ) : (
                                                        <Badge variant="secondary" className="ml-2">
                                                            {t('pages.deviceCodeList.status.offline')}
                                                        </Badge>
                                                    )}
                                                </TableCell>
                                                <TableCell>{new Date(item.createdAt).toLocaleString()}</TableCell>
                                                <TableCell>{new Date(item.updatedAt).toLocaleString()}</TableCell>
                                                <TableCell className="text-right">
                                                    <Button variant="ghost" size="icon" onClick={() => handleOpenEdit(item)}>
                                                        <Edit2 className="h-4 w-4" />
                                                    </Button>
                                                    <Button variant="destructive" size="icon" className="ml-2" disabled={item.isOnline} onClick={() => handleDelete(item.id)}>
                                                        <Trash2 className="h-4 w-4" />
                                                    </Button>
                                                </TableCell>
                                            </TableRow>
                                        ))
                                    )}
                                </TableBody>
                            </Table>
                        </div>
                    )}
                </CardContent>
            </Card>

            <Dialog open={isCreateOpen} onOpenChange={setIsCreateOpen}>
                <DialogContent>
                    <DialogHeader>
                        <DialogTitle>{t('pages.deviceCodeList.createModal.title')}</DialogTitle>
                        <DialogDescription>
                            {t('pages.deviceCodeList.createModal.description')}
                        </DialogDescription>
                    </DialogHeader>
                    <div className="grid gap-4 py-4">
                        <div className="grid grid-cols-4 items-center gap-4">
                            <Label htmlFor="clientId" className="text-right">
                                {t('pages.deviceCodeList.column.clientId')}
                            </Label>
                            <Input
                                id="clientId"
                                value={createForm.clientId}
                                onChange={(e) => setCreateForm({ ...createForm, clientId: e.target.value })}
                                className="col-span-3"
                            />
                        </div>
                        <div className="grid grid-cols-4 items-center gap-4">
                            <Label htmlFor="deviceCode" className="text-right">
                                {t('pages.deviceCodeList.column.deviceCode')}
                            </Label>
                            <Input
                                id="deviceCode"
                                value={createForm.deviceCode}
                                onChange={(e) => setCreateForm({ ...createForm, deviceCode: e.target.value })}
                                className="col-span-3"
                            />
                        </div>
                    </div>
                    <DialogFooter>
                        <Button variant="outline" onClick={() => setIsCreateOpen(false)}>{t('common.cancel')}</Button>
                        <Button onClick={submitCreate} disabled={isCreating || !createForm.clientId || !createForm.deviceCode}>
                            {isCreating && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                            {t('common.save')}
                        </Button>
                    </DialogFooter>
                </DialogContent>
            </Dialog>

            <Dialog open={!!editItem} onOpenChange={(open) => !open && setEditItem(null)}>
                <DialogContent>
                    <DialogHeader>
                        <DialogTitle>{t('pages.deviceCodeList.editModal.title')}</DialogTitle>
                        <DialogDescription>
                            {t('pages.deviceCodeList.editModal.description')}
                        </DialogDescription>
                    </DialogHeader>
                    <div className="grid gap-4 py-4">
                        <div className="grid grid-cols-4 items-center gap-4">
                            <Label htmlFor="editClientId" className="text-right">
                                {t('pages.deviceCodeList.column.clientId')}
                            </Label>
                            <Input
                                id="editClientId"
                                value={editItem?.clientId || ''}
                                disabled
                                className="col-span-3"
                            />
                        </div>
                        <div className="grid grid-cols-4 items-center gap-4">
                            <Label htmlFor="editDeviceCode" className="text-right">
                                {t('pages.deviceCodeList.column.deviceCode')}
                            </Label>
                            <Input
                                id="editDeviceCode"
                                value={editForm.deviceCode}
                                onChange={(e) => setEditForm({ ...editForm, deviceCode: e.target.value })}
                                className="col-span-3"
                            />
                        </div>
                    </div>
                    <DialogFooter>
                        <Button variant="outline" onClick={() => setEditItem(null)}>{t('common.cancel')}</Button>
                        <Button onClick={submitEdit} disabled={!editForm.deviceCode}>
                            {t('common.save')}
                        </Button>
                    </DialogFooter>
                </DialogContent>
            </Dialog>
        </div>
    )
}
