
import { useState, useRef, useEffect } from "react"
import { useParams, useNavigate } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { FileIcon, FolderIcon, ArrowUp, RefreshCw, Home, ArrowLeft, Download, Upload, Loader2, CheckCircle2, XCircle, X, ChevronLeft, ChevronRight } from "lucide-react"

import { Button } from "@/components/ui/button"
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
import { useListFiles } from "@/services/hooks/undefinedController/useListFiles"
import { Skeleton } from "@/components/ui/skeleton"
import { formatBytes } from "@/lib/utils"
import { useFileTransfer, type TransferProgress } from "./use-file-transfer"
import { useToast } from "@/hooks/use-toast"

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
    const { id: sessionId } = useParams<{ id: string }>()
    const navigate = useNavigate()
    const { t } = useTranslation()
    // Empty path: Windows shows drive letters, others show root "/"
    const [currentPath, setCurrentPath] = useState<string>("")
    const [page, setPage] = useState(1)
    const pageSize = 100
    const fileInputRef = useRef<HTMLInputElement>(null)
    const { toast } = useToast()

    const { data, isLoading, refetch, isError, error } = useListFiles({
        session_id: sessionId,
        path: currentPath,
        page_no: page,
        page_count: pageSize
    })

    useEffect(() => {
        if (isError) {
            toast({
                title: t('pages.fileManager.error', 'Error'),
                description: error instanceof Error ? error.message : t('common.unknownError', 'Unknown error'),
                variant: 'destructive',
            })
        }
    }, [isError, error, toast, t])

    const { transfers, downloadFile, uploadFile, cancelTransfer, removeTransfer, closeConnection } = useFileTransfer(sessionId)

    // Cleanup WebRTC connection when leaving the page
    useEffect(() => {
        return () => {
            closeConnection()
        }
    }, [closeConnection])

    const files = data?.file_info_list || []
    const totalCount = data?.total_count || 0
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
                    <Button variant="outline" size="icon" onClick={() => navigate(`/desk/${sessionId}`)} title={t('pages.fileManager.backToDashboard', 'Back to Dashboard')}>
                        <ArrowLeft className="h-4 w-4" />
                    </Button>
                    <Button variant="ghost" size="icon" onClick={() => handleNavigate("")}>
                        <Home className="h-4 w-4" />
                    </Button>
                    <Breadcrumb>
                        <BreadcrumbList>
                            <BreadcrumbItem>
                                <BreadcrumbPage className="font-mono text-sm">
                                    {currentPath || t('pages.fileManager.myComputer', 'My Computer')}
                                </BreadcrumbPage>
                            </BreadcrumbItem>
                        </BreadcrumbList>
                    </Breadcrumb>
                </div>
                <div className="flex items-center gap-2">
                    <Button variant="outline" size="icon" onClick={handleGoUp} disabled={currentPath === ""}>
                        <ArrowUp className="h-4 w-4" />
                    </Button>
                    <Button variant="outline" size="icon" onClick={() => refetch()}>
                        <RefreshCw className="h-4 w-4" />
                    </Button>
                    <Button variant="outline" size="sm" onClick={handleUploadClick}>
                        <Upload className="h-4 w-4 mr-1" />
                        {t('pages.fileManager.upload', 'Upload')}
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
                                        {transfer.status === 'connecting' && t('pages.fileManager.connecting', 'Connecting...')}
                                        {transfer.status === 'completed' && t('pages.fileManager.completed', 'Completed')}
                                        {transfer.status === 'error' && (transfer.errorMessage || t('pages.fileManager.error', 'Error'))}
                                    </span>
                                </div>
                                {(transfer.status === 'transferring' || transfer.status === 'connecting') && (
                                    <Progress value={transfer.progress} className="h-1 mt-1" />
                                )}
                                {transfer.status === 'transferring' && transfer.speed > 0 && (
                                    <div className="flex items-center gap-2 mt-0.5 text-xs text-muted-foreground">
                                        <span>{formatBytes(transfer.speed)}/s</span>
                                        <span>
                                            {t('pages.fileManager.remaining', 'ETA')}{' '}
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
                                title={t('pages.fileManager.cancel', 'Cancel')}
                            >
                                <X className="h-3 w-3" />
                            </Button>
                        </div>
                    ))}
                </div>
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
                                <TableHead>{t('common.name', 'Name')}</TableHead>
                                <TableHead className="w-[100px]">{t('common.size', 'Size')}</TableHead>
                                <TableHead className="w-[150px]">{t('common.modified', 'Modified')}</TableHead>
                                <TableHead className="w-[80px]">{t('common.actions', 'Actions')}</TableHead>
                            </TableRow>
                        </TableHeader>
                        <TableBody>
                            {files.length === 0 && (
                                <TableRow>
                                    <TableCell colSpan={5} className="text-center h-24 text-muted-foreground">
                                        {t('common.empty', 'Empty directory')}
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
                                                title={t('pages.fileManager.download', 'Download')}
                                            >
                                                <Download className="h-3.5 w-3.5" />
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
                        {t('pages.fileManager.totalItems', '{count} items', { count: totalCount })}
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
        </div>
    )
}
