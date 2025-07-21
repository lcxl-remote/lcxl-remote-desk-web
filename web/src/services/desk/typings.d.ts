declare namespace API {
  type AudioDataFlow = 'Render' | 'Capture';

  type AudioDevice = {
    /** data flow of the device (render or capture) */
    data_flow: AudioDataFlow;
    /** is default device for this data flow? */
    default: boolean;
    /** audio device friendly name, e.g. "Speakers (Definition Audio)" */
    firendly_name: string;
    /** device id */
    id: string;
  };

  type CurrentUser = {
    access?: any;
    address?: any;
    avatar?: any;
    country?: any;
    email?: any;
    geographic?: null | Geographic;
    group?: any;
    name: string;
    notifyCount?: number;
    phone?: any;
    signature?: any;
    tags?: any;
    title?: any;
    unreadCount?: number;
    userid?: any;
  };

  type DeleteFileRequest = {
    /** Whether to delete permanently or move to trash */
    delete_permanently?: any;
    /** The path of file to be deleted */
    file_path: string;
  };

  type deleteTurnSessionParams = {
    address: string;
    interface: string;
  };

  type DeskConfig = {
    audio_device?: null | SelectedAudioDevice;
    video_device_index: number;
    /** Video encode bitrate in bps (bits per second) */
    video_encode_bps: number;
  };

  type DisplayInfo = {
    attached_to_desktop: boolean;
    desktop_coordinates: DisplayRect;
    device_name: string;
    display_device_name?: any;
    rotation: number;
  };

  type DisplayRect = {
    bottom: number;
    left: number;
    right: number;
    top: number;
  };

  type FakeCaptcha = {
    code?: number;
    status?: any;
  };

  type FakeCaptchaParams = {
    phone?: any;
  };

  type FileInfo = {
    accessed: string;
    created: string;
    err_msg?: any;
    is_dir: boolean;
    is_file: boolean;
    is_symlink: boolean;
    modified: string;
    name: string;
    path: string;
    permissions: number;
    size: number;
  };

  type FileListResponse = {
    file_info_list: FileInfo[];
    total_count: number;
  };

  type Geographic = {
    city?: null | LabelKey;
    province?: null | LabelKey;
  };

  type getTurnSessionParams = {
    address: string;
    interface: string;
  };

  type getTurnSessionStatisticsParams = {
    address: string;
    interface: string;
  };

  type InitSignalingData = {
    audio_device_list: AudioDevice[];
    ice_servers: LcxlRTCIceServer[];
    user_name: string;
    video_device_list: DisplayInfo[];
  };

  type LabelKey = {
    key?: any;
    label?: any;
  };

  type LcxlRTCIceServer = {
    credential: string;
    urls: string[];
    username: string;
  };

  type listFilesParams = {
    path: string;
    page_no: number;
    page_count: number;
    /** Minimum file size */
    min_file_size?: number;
    /** Max file size */
    max_file_size?: number;
    /** File name filtering */
    file_name?: any;
    /** New field for file extension filtering */
    file_extension?: any;
    /** Optional file extension list filtering, comma(,) separated values. */
    file_extension_list?: any;
    /** Optional time range filter for file creation. */
    start_created_time?: any;
    end_created_time?: any;
    /** Optional time range filter for file modification. */
    start_modified_time?: any;
    end_modified_time?: any;
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

  type openTerminalSessionParams = {
    /** The command to start the terminal session. with the format of "path/to/executable,arg1,arg2" */
    command: string;
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

  type RestResponseSystemSettings = {
    code: number;
    /** System settings for the application. This struct is used to load and save settings from a configuration file. */
    data?: {
      config_file_path?: string;
      db_path?: string;
      enable_ipv6?: boolean;
      listen_addr_ipv4?: string;
      listen_addr_ipv6?: string;
      log_level?: string;
      port?: number;
    };
    message?: any;
    success: boolean;
  };

  type SelectedAudioDevice = {
    audio_data_flow: AudioDataFlow;
    /** audio device id, None for default audio device */
    audio_device_id?: any;
  };

  type SignalingErrorData = {
    /** error message */
    message: string;
    /** signaling data */
    signaling_data?: any;
    /** signaling type which errors occurred. */
    signaling_type: number;
  };

  type SignalingModel = {
    /** signaling data */
    signaling_data?: any;
    /** signaling type */
    signaling_type: number;
  };

  type SystemSettings = {
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
  };

  type TerminalList = {
    /** terminal command list */
    commands: string[][];
  };

  type TurnInfo = {
    interfaces: TurnInterface[];
    port_allocated: number;
    port_capacity: number;
    software: string;
    uptime: number;
  };

  type TurnInterface = {
    /** turn server listen address */
    bind: string;
    /** external address

specify the node external address and port.
for the case of exposing the service to the outside,
you need to manually specify the server external IP
address and service listening port. */
    external: string;
    transport: TurnTransport;
  };

  type TurnSession = {
    channels: number[];
    expires: number;
    permissions: number[];
    port?: number;
    username: string;
  };

  type TurnSessionStatistics = {
    error_pkts: number;
    received_bytes: number;
    received_pkts: number;
    send_bytes: number;
    send_pkts: number;
  };

  type TurnTransport = 'tcp' | 'udp';

  type UserResponeCurrentUser = {
    data: {
      access?: any;
      address?: any;
      avatar?: any;
      country?: any;
      email?: any;
      geographic?: null | Geographic;
      group?: any;
      name: string;
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
