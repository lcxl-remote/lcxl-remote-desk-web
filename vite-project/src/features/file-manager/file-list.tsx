
import { useState, useRef, useEffect, useCallback } from "react"
import { useParams, useNavigate } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { FileIcon, FolderIcon, ArrowUp, RefreshCw, Home, ArrowLeft, Download, Upload, Loader2, CheckCircle2, XCircle, X, ChevronLeft, ChevronRight, Trash2, Info } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Label } from "@/components/ui/label"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import {
    AlertDialog,
    AlertDialogAction,
    AlertDialogCancel,
    AlertDialogContent,
    AlertDialogDescription,
    AlertDialogFooter,
    AlertDialogHeader,
    AlertDialogTitle,
} from "@/components/ui/alert-dialog"
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "@/components/ui/table"
import {
    Breadcrumb,
    BreadcrumbItem,
    BreadcrumbList,
    BreadcrumbPage,
} from "@/components/ui/breadcrumb"
import { Progress } from "@/components/ui/progress"
import { Skeleton } from "@/components/ui/skeleton"
import { formatBytes } from "@/lib/utils"
import { useFileTransfer, type TransferProgress } from "./use-file-transfer"
import { useRestrictedSession } from "@/features/desk/restricted-session"
import { useToast } from "@/hooks/use-toast"
import { deskErrorCodeEnum, startupModeEnum, type StartupMode } from "@/services/types"
import { deskErrorMessage, errorCodeOf, type ErrorCodeKeyMap } from "@/lib/desk-error-i18n"

// File browse / delete rejections the host phrases as raw English. Only the
// permission refusal gets a localized line: it is the one outcome whose backend
// text ("File delete access denied") carries no detail worth keeping. Everything
// else — an IO failure, a path that vanished — arrives as SYSTEM_ERROR with the
// real cause in the message, so it falls through and keeps that detail.
const FILE_ERROR_CODE_TO_KEY: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.PERMISSION_ERROR]: "pages.fileError.permissionDenied",
}

// Transfer failures the user can act on, and which the host's English does not
// explain on its own. Anything else — a vanished path, an OS write error —
// keeps the host's message, which names the actual file or errno.
const TRANSFER_ERROR_CODE_TO_KEY: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.PERMISSION_ERROR]: "pages.fileError.transferDenied",
    [deskErrorCodeEnum.TIMEOUT]: "pages.fileError.transferStalled",
}

function formatRemainingTime(seconds: number): string {
    const s = Math.round(seconds);
    if (s < 60) return `0:${s.toString().padStart(2, '0')}`;
    const m = Math.floor(s / 60);
    const rs = s % 60;
    if (m < 60) return `${m}:${rs.toString().padStart(2, '0')}`;
    const h = Math.floor(m / 60);
    const rm = m % 60;
    return `${h}:${rm.toString().padStart(2, '0')}:${rs.toString().padStart(2, '0')}`;
}

export default function FileList() {
    const { id: connectionId } = useParams<{ id: string }>()
    const navigate = useNavigate()
    const { t } = useTranslation()
    // Empty path: Windows shows drive letters, others show root "/"
    const [currentPath, setCurrentPath] = useState<string>("")
    const [page, setPage] = useState(1)
    const pageSize = 100
    const fileInputRef = useRef<HTMLInputElement>(null)
    const { toast } = useToast()

    // The host's startup mode, asked of the host itself rather than of the
    // server this browser is connected to: that server may be a manager or a
    // signaling server, and its own mode says nothing about the machine whose
    // drives are listed below. `null` until the host answers, and after a
    // failure — the mode only decorates a hint, so not knowing it is not an
    // error worth showing anyone.
    const [hostStartupMode, setHostStartupMode] = useState<StartupMode | null>(null)
    const showDaemonMappedDriveHint =
        currentPath === "" && hostStartupMode === startupModeEnum["service-daemon"]

    const [deleteConfirmOpen, setDeleteConfirmOpen] = useState(false)
    const [deleteDoubleConfirmOpen, setDeleteDoubleConfirmOpen] = useState(false)
    const [fileToDelete, setFileToDelete] = useState<any>(null)
    const [isPermanentDelete, setIsPermanentDelete] = useState(false)
    const {
        transfers,
        downloadFile,
        uploadFile,
        cancelTransfer,
        removeTransfer,
        listFiles,
        deleteFile,
        querySystemInfo,
        closeConnection,
    } = useFileTransfer(connectionId)
    const restricted = useRestrictedSession(connectionId)
    const canDelete = restricted.capabilityVisible("allow_file_delete")
    const [data, setData] = useState<any>(null)
    const [isLoading, setIsLoading] = useState(true)
    const [isDeleting, setIsDeleting] = useState(false)
    const listGeneration = useRef(0)

    const loadFiles = useCallback(async () => {
        if (!connectionId) return
        const generation = ++listGeneration.current
        setIsLoading(true)
        try {
            const response = await listFiles({
                path: currentPath,
                page_no: page,
                page_count: pageSize,
            })
            if (generation === listGeneration.current) {
                setData(response)
            }
        } catch (error) {
            if (generation === listGeneration.current) {
                toast({
                    title: t("pages.fileManager.error"),
                    description: deskErrorMessage(
                        t,
                        FILE_ERROR_CODE_TO_KEY,
                        errorCodeOf(error),
                        error instanceof Error ? error.message : null,
                        t("common.unknownError"),
                    ),
                    variant: "destructive",
                })
            }
        } finally {
            if (generation === listGeneration.current) {
                setIsLoading(false)
            }
        }
    }, [connectionId, currentPath, page, listFiles, toast, t])

    useEffect(() => {
        void loadFiles()
    }, [loadFiles])

    // Ask the host what it is, once per connection. A rejection is expected
    // rather than exceptional — a session holding a capped grant may not query
    // the host's system information at all — so it only leaves the mode unknown.
    useEffect(() => {
        if (!connectionId) return
        let cancelled = false
        void querySystemInfo()
            .then(info => {
                if (!cancelled) setHostStartupMode(info?.startup_mode ?? null)
            })
            .catch(() => {
                if (!cancelled) setHostStartupMode(null)
            })
        return () => { cancelled = true }
    }, [connectionId, querySystemInfo])

    useEffect(() => {
        return () => {
            listGeneration.current++
            closeConnection()
        }
    }, [closeConnection])

    const files = data?.file_info_list || []
    const totalCount = Number(data?.total_count || 0)
    const totalPages = Math.max(1, Math.ceil(totalCount / pageSize))


    const handleNavigate = (path: string) => {
        setCurrentPath(path)
        setPage(1)
    }

    // Navigate into a directory using its path from the backend
    const handleOpen = (file: any) => {
        if (file.is_dir) {
            // Use file.path from backend directly (handles Windows paths, ".." entries, etc.)
            handleNavigate(file.path)
        }
    }

    const handleGoUp = () => {
        if (currentPath === "") return
        // Find the ".." entry in the current file list and use its path
        const parentEntry = files.find((f: any) => f.name === "..")
        if (parentEntry) {
            handleNavigate(parentEntry.path)
        } else {
            // Fallback: calculate parent path string if ".." is missing due to permission error etc.
            const normalizedPath = currentPath.replace(/\\/g, '/');
            const parts = normalizedPath.split('/').filter(Boolean);
            if (parts.length <= 1) {
                // Return to root drive list for Windows or root "/" for Unix
                handleNavigate("");
            } else {
                parts.pop();
                // Determine whether it was a Windows path (e.g. C:\...) by looking for :
                if (currentPath.includes(':') && !currentPath.includes('/')) {
                    handleNavigate(parts.join('\\') + '\\');
                } else if (currentPath.startsWith('/')) {
                    handleNavigate('/' + parts.join('/'));
                } else {
                    handleNavigate(parts.join('/'));
                }
            }
        }
    }

    const handleDownload = (e: React.MouseEvent, file: any) => {
        e.stopPropagation()
        if (file.is_dir) return
        // Use file.path from backend directly
        downloadFile(file.path, file.name)
    }

    const handleDeleteClick = (e: React.MouseEvent, file: any) => {
        e.stopPropagation()
        setFileToDelete(file)
        setIsPermanentDelete(false)
        setDeleteConfirmOpen(true)
    }

    const confirmDelete = () => {
        if (isPermanentDelete) {
            setDeleteConfirmOpen(false)
            setDeleteDoubleConfirmOpen(true)
        } else {
            executeDelete()
        }
    }

    const executeDelete = async () => {
        if (!fileToDelete || !connectionId || isDeleting) return
        setIsDeleting(true)
        try {
            await deleteFile({
                file_path: fileToDelete.path,
                delete_permanently: isPermanentDelete,
            })
            toast({
                title: t("pages.fileManager.deleteSuccess"),
                variant: "default",
            })
            await loadFiles()
            setDeleteConfirmOpen(false)
            setDeleteDoubleConfirmOpen(false)
            setFileToDelete(null)
        } catch (error) {
            // The title states what failed and the description says why. Putting
            // the reason in both (the old `删除失败: {{error}}` template) printed
            // it twice, since the toast renders the two lines separately.
            toast({
                title: t("pages.fileManager.deleteFailed"),
                description: deskErrorMessage(
                    t,
                    FILE_ERROR_CODE_TO_KEY,
                    errorCodeOf(error),
                    error instanceof Error ? error.message : null,
                    t("common.unknownError"),
                ),
                variant: "destructive",
            })
        } finally {
            setIsDeleting(false)
        }
    }

    const handleUploadClick = () => {
        fileInputRef.current?.click()
    }


    const handleFileSelected = (e: React.ChangeEvent<HTMLInputElement>) => {
        const files = e.target.files
        if (!files || files.length === 0) return

        for (let i = 0; i < files.length; i++) {
            uploadFile(currentPath, files[i])
        }

        // Reset input
        if (fileInputRef.current) {
            fileInputRef.current.value = ''
        }
    }

    const getTransferIcon = (transfer: TransferProgress) => {
        switch (transfer.status) {
            case 'connecting':
            case 'transferring':
                return <Loader2 className="h-3 w-3 animate-spin" />
            case 'completed':
                return <CheckCircle2 className="h-3 w-3 text-green-500" />
            case 'error':
                return <XCircle className="h-3 w-3 text-red-500" />
        }
    }

    return (
        <div className="space-y-4 h-full flex flex-col">
            <div className="flex items-center justify-between px-4 py-2 border-b">
                <div className="flex items-center gap-2 overflow-hidden">
                    <Button variant="outline" size="icon" onClick={() => navigate(`/desk/${connectionId}`)} title={t('pages.fileManager.backToDashboard')}>
                        <ArrowLeft className="h-4 w-4" />
                    </Button>
                    <Button variant="ghost" size="icon" onClick={() => handleNavigate("")}>
                        <Home className="h-4 w-4" />
                    </Button>
                    <Breadcrumb>
                        <BreadcrumbList>
                            <BreadcrumbItem>
                                <BreadcrumbPage className="font-mono text-sm">
                                    {currentPath || t('pages.fileManager.myComputer')}
                                </BreadcrumbPage>
                            </BreadcrumbItem>
                        </BreadcrumbList>
                    </Breadcrumb>
                </div>
                <div className="flex items-center gap-2">
                    <Button variant="outline" size="icon" onClick={handleGoUp} disabled={currentPath === ""}>
                        <ArrowUp className="h-4 w-4" />
                    </Button>
                    <Button variant="outline" size="icon" onClick={() => void loadFiles()}>
                        <RefreshCw className="h-4 w-4" />
                    </Button>
                    <Button variant="outline" size="sm" onClick={handleUploadClick}>
                        <Upload className="h-4 w-4 mr-1" />
                        {t('pages.fileManager.upload')}
                    </Button>
                    <input
                        ref={fileInputRef}
                        type="file"
                        className="hidden"
                        multiple
                        onChange={handleFileSelected}
                    />
                </div>
            </div>

            {/* Transfer progress panel */}
            {transfers.length > 0 && (
                <div className="mx-4 space-y-2">
                    {transfers.map((transfer) => (
                        <div key={transfer.transferId} className="flex items-center gap-3 px-3 py-2 rounded-md border bg-muted/30 text-sm">
                            {getTransferIcon(transfer)}
                            <div className="flex-1 min-w-0">
                                <div className="flex items-center gap-2">
                                    <span className="truncate font-medium">{transfer.fileName}</span>
                                    <span className="text-xs text-muted-foreground shrink-0">
                                        {transfer.direction === 'download' ? '↓' : '↑'}
                                        {' '}
                                        {transfer.status === 'transferring' && `${transfer.progress}%`}
                                        {transfer.status === 'connecting' && t('pages.fileManager.connecting')}
                                        {transfer.status === 'completed' && t('pages.fileManager.completed')}
                                        {transfer.status === 'error' && deskErrorMessage(
                                            t,
                                            TRANSFER_ERROR_CODE_TO_KEY,
                                            transfer.errorCode,
                                            transfer.errorMessage,
                                            t('pages.fileManager.error'),
                                        )}
                                    </span>
                                </div>
                                {(transfer.status === 'transferring' || transfer.status === 'connecting') && (
                                    <Progress value={transfer.progress} className="h-1 mt-1" />
                                )}
                                {transfer.status === 'transferring' && transfer.speed > 0 && (
                                    <div className="flex items-center gap-2 mt-0.5 text-xs text-muted-foreground">
                                        <span>{formatBytes(transfer.speed)}/s</span>
                                        <span>
                                            {t('pages.fileManager.remaining')}{' '}
                                            {formatRemainingTime(Math.max(0, transfer.remainingSeconds))}
                                        </span>
                                    </div>
                                )}
                            </div>
                            {transfer.fileSize > 0 && (
                                <span className="text-xs text-muted-foreground shrink-0">
                                    {formatBytes(transfer.transferredBytes)} / {formatBytes(transfer.fileSize)}
                                </span>
                            )}
                            <Button
                                variant="ghost"
                                size="icon"
                                className="h-6 w-6 shrink-0"
                                onClick={() => {
                                    if (transfer.status === 'completed' || transfer.status === 'error') {
                                        removeTransfer(transfer.transferId);
                                    } else {
                                        cancelTransfer(transfer.transferId);
                                    }
                                }}
                                title={t('pages.fileManager.cancel')}
                            >
                                <X className="h-3 w-3" />
                            </Button>
                        </div>
                    ))}
                </div>
            )}

            {showDaemonMappedDriveHint && (
                <Alert className="mx-4">
                    <Info className="h-4 w-4" />
                    <AlertTitle>{t('pages.fileManager.daemonMappedDriveHint.title')}</AlertTitle>
                    <AlertDescription>
                        {t('pages.fileManager.daemonMappedDriveHint.description')}
                    </AlertDescription>
                </Alert>
            )}

            <div className="flex-1 overflow-auto">
                {isLoading ? (
                    <div className="space-y-2 p-4">
                        {[...Array(5)].map((_, i) => (
                            <Skeleton key={i} className="h-10 w-full" />
                        ))}
                    </div>
                ) : (
                    <Table>
                        <TableHeader>
                            <TableRow>
                                <TableHead className="w-[50px]"></TableHead>
                                <TableHead>{t('common.name')}</TableHead>
                                <TableHead className="w-[100px]">{t('common.size')}</TableHead>
                                <TableHead className="w-[150px]">{t('common.modified')}</TableHead>
                                <TableHead className="w-[80px]">{t('common.actions')}</TableHead>
                            </TableRow>
                        </TableHeader>
                        <TableBody>
                            {files.length === 0 && (
                                <TableRow>
                                    <TableCell colSpan={5} className="text-center h-24 text-muted-foreground">
                                        {t('common.empty')}
                                    </TableCell>
                                </TableRow>
                            )}
                            {files.map((file: any) => (
                                <TableRow
                                    key={file.name}
                                    className="cursor-pointer hover:bg-muted/50"
                                    onClick={() => handleOpen(file)}
                                >
                                    <TableCell>
                                        {file.is_dir ? (
                                            <FolderIcon className="h-4 w-4 text-blue-500" />
                                        ) : (
                                            <FileIcon className="h-4 w-4 text-gray-500" />
                                        )}
                                    </TableCell>
                                    <TableCell className="font-medium">{file.name}</TableCell>
                                    <TableCell>{file.is_dir ? '-' : formatBytes(file.size)}</TableCell>
                                    <TableCell>{new Date(file.modified).toLocaleString()}</TableCell>
                                    <TableCell>
                                        {!file.is_dir && (
                                            <Button
                                                variant="ghost"
                                                size="icon"
                                                className="h-7 w-7"
                                                onClick={(e) => handleDownload(e, file)}
                                                title={t('pages.fileManager.download')}
                                            >
                                                <Download className="h-3.5 w-3.5" />
                                            </Button>
                                        )}
                                        {file.name !== ".." && canDelete && (
                                            <Button
                                                variant="ghost"
                                                size="icon"
                                                className="h-7 w-7 text-red-500 hover:text-red-700 hover:bg-red-50"
                                                onClick={(e) => handleDeleteClick(e, file)}
                                                title={t('pages.fileManager.delete')}
                                                disabled={isDeleting}
                                            >
                                                {isDeleting && fileToDelete?.path === file.path ? (
                                                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                                                ) : (
                                                    <Trash2 className="h-3.5 w-3.5" />
                                                )}
                                            </Button>
                                        )}
                                    </TableCell>
                                </TableRow>
                            ))}
                        </TableBody>
                    </Table>
                )}
            </div>
            {/* Pagination */}
            {totalPages > 1 && (
                <div className="flex items-center justify-between px-4 py-2 border-t">
                    <span className="text-sm text-muted-foreground">
                        {t('pages.fileManager.totalItems', { count: totalCount })}
                    </span>
                    <div className="flex items-center gap-2">
                        <Button
                            variant="outline"
                            size="icon"
                            className="h-7 w-7"
                            disabled={page <= 1}
                            onClick={() => setPage(p => p - 1)}
                        >
                            <ChevronLeft className="h-4 w-4" />
                        </Button>
                        <span className="text-sm">
                            {page} / {totalPages}
                        </span>
                        <Button
                            variant="outline"
                            size="icon"
                            className="h-7 w-7"
                            disabled={page >= totalPages}
                            onClick={() => setPage(p => p + 1)}
                        >
                            <ChevronRight className="h-4 w-4" />
                        </Button>
                    </div>
                </div>
            )}

            <AlertDialog open={deleteConfirmOpen} onOpenChange={setDeleteConfirmOpen}>
                <AlertDialogContent>
                    <AlertDialogHeader>
                        <AlertDialogTitle>{t('pages.fileManager.deleteConfirm.title')}</AlertDialogTitle>
                        <AlertDialogDescription>
                            {t('pages.fileManager.deleteConfirm.description')}
                            <br />
                            <span className="font-mono text-xs break-all mt-2 block p-2 bg-muted rounded">
                                {fileToDelete?.path}
                            </span>
                        </AlertDialogDescription>
                    </AlertDialogHeader>
                    <div className="flex items-center space-x-2 py-4">
                        <Checkbox
                            id="permanent-delete"
                            checked={isPermanentDelete}
                            onCheckedChange={(checked) => setIsPermanentDelete(!!checked)}
                        />
                        <Label htmlFor="permanent-delete" className="text-sm font-medium leading-none cursor-pointer">
                            {t('pages.fileManager.deletePermanently')}
                        </Label>
                    </div>
                    <AlertDialogFooter>
                        <AlertDialogCancel>{t('common.cancel')}</AlertDialogCancel>
                        <AlertDialogAction onClick={confirmDelete} className={isPermanentDelete ? "bg-red-600 hover:bg-red-700" : ""}>
                            {t('common.confirm')}
                        </AlertDialogAction>
                    </AlertDialogFooter>
                </AlertDialogContent>
            </AlertDialog>

            <AlertDialog open={deleteDoubleConfirmOpen} onOpenChange={setDeleteDoubleConfirmOpen}>
                <AlertDialogContent>
                    <AlertDialogHeader>
                        <AlertDialogTitle className="text-red-600 text-lg font-bold">
                            {t('pages.fileManager.deleteDoubleConfirm.title')}
                        </AlertDialogTitle>
                        <AlertDialogDescription>
                            {t('pages.fileManager.deleteDoubleConfirm.description')}
                        </AlertDialogDescription>
                    </AlertDialogHeader>
                    <AlertDialogFooter>
                        <AlertDialogCancel>{t('common.cancel')}</AlertDialogCancel>
                        <AlertDialogAction onClick={executeDelete} className="bg-red-600 hover:bg-red-700">
                            {t('common.confirm')}
                        </AlertDialogAction>
                    </AlertDialogFooter>
                </AlertDialogContent>
            </AlertDialog>
        </div>
    )
}
