declare namespace API {
  type CurrentUser = {
    access?: any;
    address?: any;
    avatar?: any;
    country?: any;
    email?: any;
    geographic?: null | Geographic;
    group?: any;
    name?: any;
    notifyCount?: number;
    phone?: any;
    signature?: any;
    tags?: any;
    title?: any;
    unreadCount?: number;
    userid?: any;
  };

  type DeleteFilePath = {
    /** The directory path of file to be deleted */
    dir_path: string;
    /** The name of file to be deleted */
    file_name: string;
  };

  type DeleteFileRequest = {
    /** Whether to delete permanently or move to trash */
    delete_permanently?: any;
    /** The directory path of file to be deleted */
    dir_path: string;
    /** The name of file to be deleted */
    file_name: string;
    /** Force delete the file even if it is not duplicates. This option should be used with caution */
    force_delete?: any;
  };

  type DeleteFilesRequest = {
    /** Whether to delete permanently or move to trash */
    delete_permanently?: any;
    /** The directory path of file to be deleted */
    files: DeleteFilePath[];
    /** Force delete the file even if it is not duplicates. This option should be used with caution */
    force_delete?: any;
  };

  type DeleteTrashFilePath = {
    /** The directory path of trash file */
    dir_path: string;
    /** The name of trash file */
    file_name: string;
  };

  type DeleteTrashFileRequest = {
    /** The directory path of trash file */
    dir_path: string;
    /** The name of trash file */
    file_name: string;
  };

  type DeleteTrashFilesRequest = {
    /** The directory path of file to be deleted */
    files: DeleteTrashFilePath[];
  };

  type FakeCaptcha = {
    code?: number;
    status?: any;
  };

  type FakeCaptchaParams = {
    phone?: any;
  };

  type FileInfo = {
    /** Dir path of the directory containing the file */
    dir_path: string;
    /** File extension */
    file_extension?: any;
    /** File name */
    file_name: string;
    /** File path */
    file_path: string;
    /** Inode info */
    inode_info: InodeInfo;
    /** scan_time is the time when the file was last scanned */
    scan_time: string;
    /** version is the version of the file, used to track changes */
    version: number;
  };

  type FileInfoList = {
    /** File info list */
    file_info_list: FileInfoWithMd5Count[];
    /** Total file count */
    total_count: number;
  };

  type FileInfoWithMd5Count = {
    /** File info */
    file_info: FileInfo;
    /** Optional filter md5 count */
    filter_md5_count?: any;
    /** Md5 count */
    md5_count: number;
  };

  type Geographic = {
    city?: null | LabelKey;
    province?: null | LabelKey;
  };

  type InodeInfo = {
    created: string;
    /** Device ID */
    dev_id: number;
    gid: number;
    /** Inode number */
    inode: number;
    /** File md5 */
    md5?: any;
    modified: string;
    nlink: number;
    permissions: number;
    /** File size */
    size: number;
    uid: number;
  };

  type LabelKey = {
    key?: any;
    label?: any;
  };

  type listFilesParams = {
    /** Page number, start from 1 */
    page_no: number;
    /** Page count, must be greater than 0 */
    page_count: number;
    /** Minimum file size */
    min_file_size?: number;
    /** Max file size */
    max_file_size?: number;
    /** Dir path of the directory containing the file */
    dir_path?: any;
    /** File name filtering */
    file_name?: any;
    /** New field for file extension filtering */
    file_extension?: any;
    /** Optional file extension list filtering, comma(,) separated values. */
    file_extension_list?: any;
    /** MD5 hash of the file content, used for filtering files by their content. */
    md5?: any;
    /** Optional time range filter for file creation. */
    start_created_time?: any;
    end_created_time?: any;
    /** Optional time range filter for file modification. */
    start_modified_time?: any;
    end_modified_time?: any;
    /** Minimum file md5 count */
    min_md5_count?: number;
    /** Max file md5 count */
    max_md5_count?: number;
    /** Optional order by field. */
    order_by?: any;
    /** Optional order direction, true for ascending, false for descending. Default is descending. */
    order_asc?: any;
    /** Optional filter for duplicate files in a specific directory path. If set, if files within this directory duplicate those outside of it, they will be displayed. */
    filter_dup_file_by_dir_path?: any;
  };

  type ListSettings = {
    /** Dir path of the directory containing the file */
    dir_path?: any;
    end_created_time?: any;
    end_modified_time?: any;
    /** New field for file extension filtering */
    file_extension?: any;
    /** Optional file extension list filtering, comma(,) separated values. */
    file_extension_list?: any;
    /** File name filtering */
    file_name?: any;
    /** Optional filter for duplicate files in a specific directory path. If set, if files within this directory duplicate those outside of it, they will be displayed. */
    filter_dup_file_by_dir_path?: any;
    /** Max file size */
    max_file_size?: number;
    /** Max file md5 count */
    max_md5_count?: number;
    /** MD5 hash of the file content, used for filtering files by their content. */
    md5?: any;
    /** Minimum file size */
    min_file_size?: number;
    /** Minimum file md5 count */
    min_md5_count?: number;
    /** Optional order direction, true for ascending, false for descending. Default is descending. */
    order_asc?: any;
    /** Optional order by field. */
    order_by?: any;
    /** Page count, must be greater than 0 */
    page_count: number;
    /** Page number, start from 1 */
    page_no: number;
    /** Optional time range filter for file creation. */
    start_created_time?: any;
    /** Optional time range filter for file modification. */
    start_modified_time?: any;
  };

  type listTrashFilesParams = {
    /** Page number, start from 1 */
    page_no: number;
    /** Page count, must be greater than 0 */
    page_count: number;
    /** Minimum file size */
    min_file_size?: number;
    /** Max file size */
    max_file_size?: number;
    /** Dir path of the directory containing the file */
    dir_path?: any;
    /** File name filtering */
    file_name?: any;
    /** New field for file extension filtering */
    file_extension?: any;
    /** Optional file extension list filtering, comma(,) separated values. */
    file_extension_list?: any;
    /** MD5 hash of the file content, used for filtering files by their content. */
    md5?: any;
    /** Optional time range filter for file creation. */
    start_created_time?: any;
    end_created_time?: any;
    /** Optional time range filter for file modification. */
    start_modified_time?: any;
    end_modified_time?: any;
    /** Optional time range filter for file remove. */
    start_removed_time?: any;
    end_removed_time?: any;
    /** Optional order by field. */
    order_by?: any;
    /** Optional order direction, true for ascending, false for descending. Default is descending. */
    order_asc?: any;
  };

  type LoginParams = {
    autoLogin: boolean;
    password: string;
    type: string;
    username: string;
  };

  type LoginResult = {
    currentAuthority: string;
    status: string;
    type: string;
  };

  type NoLogintUser = {
    isLogin: boolean;
  };

  type NoticeIconItem = {
    avatar?: any;
    datetime?: any;
    description?: any;
    extra?: any;
    id?: any;
    key?: any;
    read?: any;
    status?: any;
    title?: any;
    type?: null | NoticeIconItemType;
  };

  type NoticeIconItemType = 'notification' | 'message' | 'event';

  type NoticeIconList = {
    data?: any;
    success: boolean;
    total: number;
  };

  type PasswordParams = {
    /** New password (optional) */
    new_password?: any;
    /** New username (optional) */
    new_username?: any;
    /** Old password */
    password: string;
    /** Old username */
    username: string;
  };

  type RestoreTrashFilePath = {
    /** The directory path of trash file */
    dir_path: string;
    /** The name of trash file */
    file_name: string;
  };

  type RestoreTrashFilesRequest = {
    /** The directory path of file to be restore */
    files: RestoreTrashFilePath[];
  };

  type RestResponseI64 = {
    code: number;
    data?: number;
    message?: any;
    success: boolean;
  };

  type RestResponseListSettings = {
    code: number;
    /** Query parameters for listing files. */
    data?: {
      dir_path?: any;
      end_created_time?: any;
      end_modified_time?: any;
      file_extension?: any;
      file_extension_list?: any;
      file_name?: any;
      filter_dup_file_by_dir_path?: any;
      max_file_size?: number;
      max_md5_count?: number;
      md5?: any;
      min_file_size?: number;
      min_md5_count?: number;
      order_asc?: any;
      order_by?: any;
      page_count: number;
      page_no: number;
      start_created_time?: any;
      start_modified_time?: any;
    };
    message?: any;
    success: boolean;
  };

  type RestResponseScanSettings = {
    code: number;
    /** Scan settings */
    data?: {
      ignore_paths?: any;
      include_file_extensions?: any;
      max_file_size?: number;
      min_file_size?: number;
      scan_path?: string;
    };
    message?: any;
    success: boolean;
  };

  type RestResponseScanStatus = {
    code: number;
    /** Scan status structure to keep track of the progress and state of a file scan operation. */
    data?: {
      current_file_info?: null | FileInfo;
      scan_request?: null | ScanSettings;
      scanned_file_count: number;
      start_time?: any;
      started: boolean;
    };
    message?: any;
    success: boolean;
  };

  type RestResponseSystemSettings = {
    code: number;
    /** System settings for the application. This struct is used to load and save settings from a configuration file. */
    data?: {
      clear_trash_interval_s?: number;
      config_file_path?: string;
      db_path?: string;
      enable_ipv6?: boolean;
      listen_addr_ipv4?: string;
      listen_addr_ipv6?: string;
      log_level?: string;
      port?: number;
      trash_path?: string;
    };
    message?: any;
    success: boolean;
  };

  type RestResponseTrashListSettings = {
    code: number;
    /** Query parameters for listing files. */
    data?: {
      dir_path?: any;
      end_created_time?: any;
      end_modified_time?: any;
      end_removed_time?: any;
      file_extension?: any;
      file_extension_list?: any;
      file_name?: any;
      max_file_size?: number;
      md5?: any;
      min_file_size?: number;
      order_asc?: any;
      order_by?: any;
      page_count: number;
      page_no: number;
      start_created_time?: any;
      start_modified_time?: any;
      start_removed_time?: any;
    };
    message?: any;
    success: boolean;
  };

  type ScanSettings = {
    /** Ignore path to ignore during scan. If not provided, no paths will be ignored. */
    ignore_paths?: any;
    /** Optional list of file extensions to include in the scan. If not provided, all files will be scanned. */
    include_file_extensions?: any;
    /** Maximum file size in bytes to include in the scan. If not provided, there is no maximum size limit. */
    max_file_size?: number;
    /** Minimum file size in bytes to include in the scan. If not provided, there is no minimum size limit. */
    min_file_size?: number;
    /** Scan path */
    scan_path?: string;
  };

  type ScanStatus = {
    current_file_info?: null | FileInfo;
    scan_request?: null | ScanSettings;
    /** Number of files scanned so far. */
    scanned_file_count: number;
    /** Start time of the scan. */
    start_time?: any;
    /** Indicates whether the scan has started. */
    started: boolean;
  };

  type SystemSettings = {
    /** interval in seconds to clear trash */
    clear_trash_interval_s?: number;
    /** Path to the configuration file. If not specified, a new one will be created in the "conf" directory. */
    config_file_path?: string;
    /** Path to the database file. If not specified, a new one will be created in the "conf" directory. */
    db_path?: string;
    /** Enable IPv6 support */
    enable_ipv6?: boolean;
    /** listen ipv4 address for the server to bind to */
    listen_addr_ipv4?: string;
    /** listen ipv6 address for the server to bind to */
    listen_addr_ipv6?: string;
    /** access logs are printed with the INFO level so ensure it is enabled by default */
    log_level?: string;
    /** port number for the server to bind to */
    port?: number;
    /** trash path for deleted files */
    trash_path?: string;
  };

  type TrashFileInfo = {
    /** Created time */
    created: string;
    /** Dir path of the directory containing the file */
    dir_path: string;
    /** File extension */
    file_extension?: any;
    /** File name */
    file_name: string;
    gid: number;
    /** File md5 */
    md5: string;
    /** Modified time */
    modified: string;
    permissions: number;
    /** Remove time */
    remove_time: string;
    /** File size */
    size: number;
    uid: number;
  };

  type TrashFileInfoList = {
    /** Total trash file count */
    total_count: number;
    /** Trash file info list */
    trash_file_info_list: TrashFileInfo[];
  };

  type TrashListSettings = {
    /** Dir path of the directory containing the file */
    dir_path?: any;
    end_created_time?: any;
    end_modified_time?: any;
    end_removed_time?: any;
    /** New field for file extension filtering */
    file_extension?: any;
    /** Optional file extension list filtering, comma(,) separated values. */
    file_extension_list?: any;
    /** File name filtering */
    file_name?: any;
    /** Max file size */
    max_file_size?: number;
    /** MD5 hash of the file content, used for filtering files by their content. */
    md5?: any;
    /** Minimum file size */
    min_file_size?: number;
    /** Optional order direction, true for ascending, false for descending. Default is descending. */
    order_asc?: any;
    /** Optional order by field. */
    order_by?: any;
    /** Page count, must be greater than 0 */
    page_count: number;
    /** Page number, start from 1 */
    page_no: number;
    /** Optional time range filter for file creation. */
    start_created_time?: any;
    /** Optional time range filter for file modification. */
    start_modified_time?: any;
    /** Optional time range filter for file remove. */
    start_removed_time?: any;
  };

  type UserResponeCurrentUser = {
    data: {
      access?: any;
      address?: any;
      avatar?: any;
      country?: any;
      email?: any;
      geographic?: null | Geographic;
      group?: any;
      name?: any;
      notifyCount?: number;
      phone?: any;
      signature?: any;
      tags?: any;
      title?: any;
      unreadCount?: number;
      userid?: any;
    };
    errorCode: number;
    errorMessage: string;
    success: boolean;
  };

  type UserResponeNoLogintUser = {
    data: { isLogin: boolean };
    errorCode: number;
    errorMessage: string;
    success: boolean;
  };
}
