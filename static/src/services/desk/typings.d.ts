declare namespace API {
  type AudioDataFlow = 'Render' | 'Capture';

  type AudioDevice = {
    /** Data flow of the device (render or capture) */
    data_flow: AudioDataFlow;
    /** Is default device for this data flow? */
    default: boolean;
    /** Audio device friendly name, e.g. "Speakers (Definition Audio)" */
    firendly_name: string;
    /** Device id */
    id: string;
  };

  type CpuInfo = {
    /** CPU brand */
    brand: string;
    /** CPU frequency in MHz */
    frequency: number;
    /** CPU name */
    name: string;
    /** CPU usage percentage */
    usage: number;
    /** CPU vendor ID */
    vendor_id: string;
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
    session_id?: any;
  };

  type deleteTurnSessionParams = {
    address: string;
    interface: string;
  };

  type DeskSettings = {
    /** Enable adaptive web page resolution */
    adaptive_web_page_resolution?: boolean;
    /** Selected audio capture device */
    audio_capture?: any;
    audio_device?: null | SelectedAudioDevice;
    /** Audio encoder name, None for auto detection */
    audio_encoder?: any;
    /** Display name */
    display_name?: any;
    /** Enable D3D debug mode */
    enable_d3d_debug?: boolean;
    h264_encoder?: null | H264EncoderSettings;
    /** Selected image capture device */
    image_capture?: any;
    opus_encoder?: null | OpusEncoderSettings;
    private_screen?: PrivateScreenSettings;
    /** Enable mouse display on the screen */
    show_mouse?: boolean;
    /** Video device index */
    video_device_index?: number;
    /** Video encode bitrate in bps (bits per second) */
    video_encode_bps?: number;
    /** Video encoder name, None for auto detection */
    video_encoder?: any;
    /** Video frame rate (e.g., 30 fps) */
    video_fps?: number;
    /** Video zoom ratio (e.g., 50 for 50% zoom) */
    video_zoom_ratio?: number;
    vp8_encoder?: null | VpxEncoderSettings;
    vp9_encoder?: null | VpxEncoderSettings;
  };

  type DisplayInfo = {
    /** Is the display attached to the desktop */
    attached_to_desktop: boolean;
    /** Display device rect coordinates on the desktop */
    desktop_coordinates: DisplayRect;
    /** Display device name, e.g. "\\.\DISPLAY1" */
    device_name: string;
    /** Display device friendly name, e.g. "Generic PnP Monitor" */
    display_device_name?: any;
    /** Supported display resolutions (width, height) */
    resolutions: Resolution[];
    /** Display rotation angle in degrees, e.g. 0, 90, 180, 270 */
    rotation: number;
  };

  type DisplayRect = {
    /** Bottom coordinate of the rectangle */
    bottom: number;
    /** Left coordinate of the rectangle */
    left: number;
    /** Right coordinate of the rectangle */
    right: number;
    /** Top coordinate of the rectangle */
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

  type H264EncoderSettings = {
    /** Bitrate in bps (bits per second), default is 10,000,000 bps (10 Mbps) */
    bps?: number;
    /** Group of Pictures, default is 0, which means the encoder will decide the value. */
    gop?: number;
  };

  type HotkeySettings = {
    /** HOT_KEY_MODIFIERS */
    fsmodifiers?: number;
    vk?: number;
  };

  type InitSignalingData = {
    /** Audio device list */
    audio_device_list: Record<string, any>;
    /** Audio encoder list */
    audio_encoder_list: string[];
    /** Current desk settings */
    desk_settings: DeskSettings;
    /** ICE servers to use for signaling. */
    ice_servers: LcxlRTCIceServer[];
    /** User name for signaling. */
    user_name: string;
    /** Video device list */
    video_device_list: Record<string, any>;
    /** Video encoder list */
    video_encoder_list: string[];
  };

  type KeyboardEventData = {
    /** whether the key is a system key */
    alt_key: boolean;
    /** key code, e.g. "KeyA", "Enter", see https://developer.mozilla.org/zh-CN/docs/Web/API/KeyboardEvent/code#code_values */
    code: string;
    /** whether the ctrl key is pressed */
    ctrl_key: boolean;
    /** keyboard event type, e.g. "keydown", "keyup", "keypress" */
    event: string;
    /** whether the key is composing */
    is_composing: boolean;
    /** key value, e.g. "a", see https://developer.mozilla.org/zh-CN/docs/Web/API/UI_Events/Keyboard_event_key_values */
    key: string;
    /** physical key code, e.g. 65 for "a", see https://developer.mozilla.org/zh-CN/docs/Web/API/KeyboardEvent/keyCode */
    key_code: number;
    /** location of the key on the keyboard */
    location: number;
    /** whether the meta key is pressed */
    meta_key: boolean;
    /** whether the key is repeated */
    repeat: boolean;
    /** whether the shift key is pressed */
    shift_key: boolean;
  };

  type LabelKey = {
    key?: any;
    label?: any;
  };

  type LcxlRTCIceServer = {
    /** Credential for the ICE server, if any. */
    credential: string;
    /** List of URLs associated with the ICE server, e.g. ["stun:stun.l.google.com:19302"] */
    urls: string[];
    /** Username for the ICE server, if any. */
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
    /** Session ID for remote desk */
    session_id?: any;
  };

  type listTerminalParams = {
    /** session id */
    session_id: string;
  };

  type LoginParams = {
    autoLogin: boolean;
    password: string;
    type: string;
    username: string;
  };

  type LoginResult = {
    /** return api version of signal/desk/manage server */
    api_version: number;
    currentAuthority: string;
    status: string;
    type: string;
  };

  type MouseEventData = {
    /** Returns true if the alt key was down when the mouse event was fired. */
    alt_key: boolean;
    /** The button number that was pressed or released (if applicable) when the mouse event was fired. */
    button: number;
    /** The buttons being pressed (if any) when the mouse event was fired. */
    buttons: number;
    /** The distance that the mouse wheel has been rotated around the X axis (if applicable) when the mouse event was fired. */
    delta_x: number;
    /** The distance that the mouse wheel has been rotated around the Y axis (if applicable) when the mouse event was fired. */
    delta_y: number;
    /** mouse event type, e.g. "mousemove", "mousedown", "mouseup", "click", "dblclick", "contextmenu", "wheel" */
    event: string;
    /** mouse x coordinate(relative to the viewport) */
    x: number;
    /** mouse y coordinate(relative to the viewport) */
    y: number;
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

  type openSignalingHandleParams = {
    /** The version of the API. This is a simple integer that increments when API is changed. */
    api_version: number;
    /** The build number of the server. */
    build_number: number;
    /** The commit hash of the server. */
    commit_hash: string;
    /** Remote desk type associated with the version. */
    remote_desk_type: RemoteDeskTypeEnum;
    /** Operation system associated with the version. */
    operation_system: OperationSystemEnum;
    /** Display name of the remote desk. */
    display_name?: any;
  };

  type openTerminalSessionParams = {
    /** The command to start the terminal session. with the format of "path/to/executable,arg1,arg2" */
    command: string;
    /** session id */
    session_id: string;
  };

  type OperationSystemEnum = 'Windows' | 'Linux' | 'Mac' | 'Android' | 'Ios' | 'Web' | 'Other';

  type OpusEncoderSettings = {
    application?: string;
    channels?: number;
    sample_rate?: number;
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

  type PrivateScreenSettings = {
    hotkey?: null | HotkeySettings;
    /** Optional image path for the private screen background */
    image_path?: any;
    /** Optional window extended style for the private screen window */
    window_ex_style?: number;
    /** Optional window style for the private screen window */
    window_style?: number;
  };

  type RemoteDeskTypeEnum = 'browser' | 'server' | 'signal' | 'manager';

  type Resolution = {
    /** Height of the resolution in pixels */
    height: number;
    /** Width of the resolution in pixels */
    width: number;
  };

  type RestResponseSystemInfo = {
    code: number;
    /** System information */
    data?: {
      cpus: CpuInfo[];
      host_name?: any;
      kernel_version?: any;
      name?: any;
      os_version?: any;
      startup_mode: string;
      total_memory: number;
      total_swap: number;
      used_memory: number;
      used_swap: number;
    };
    message?: any;
    success: boolean;
  };

  type RestResponseSystemSettings = {
    code: number;
    /** System settings for the application. This struct is used to load and save settings from a configuration file. */
    data?: {
      client_id?: any;
      enable_ipv6?: boolean;
      listen_addr_ipv4?: string;
      listen_addr_ipv6?: string;
      locale?: any;
      log_level?: string;
      open_browser_on_startup?: boolean;
      port?: number;
      signaling_url?: any;
      telemetry_consent?: any;
      traceback?: boolean;
    };
    message?: any;
    success: boolean;
  };

  type RestResponseTelemetryStatus = {
    code: number;
    data?: { consented?: any; needed: boolean };
    message?: any;
    success: boolean;
  };

  type SelectedAudioDevice = {
    /** Audio data flow (render or capture) */
    audio_data_flow: AudioDataFlow;
    /** Audio device id, None for default audio device */
    audio_device_id?: any;
  };

  type SessionList = {
    /** Current session ID */
    current_session_id: string;
    /** Session map */
    session_map: Record<string, any>;
  };

  type SessionModel = {
    /** IP address of the session */
    ip?: any;
    /** Session ID */
    session_id: string;
    /** Version info of the session */
    version_info: VersionInfo;
  };

  type SignalingModel = {
    /** From session id, if None, means from signal server */
    from_session_id?: any;
    /** Request id */
    request_id: string;
    response_state?: null | SignalingResponseState;
    /** Signaling data */
    signaling_data?: any;
    /** Signaling type */
    signaling_type: SignalingType;
    /** To session id, if None, means to signal server */
    to_session_id?: any;
  };

  type SignalingResponseState = {
    /** error code

see alse: desk_utils::DeskErrorCode */
    error_code: number;
    /** error message */
    message?: any;
  };

  type SignalingType = integer;

  type SignalRequestControlData = {
    /** whether the control request is accepted */
    accept?: boolean;
    /** whether to accept clipboard sync */
    accept_clipboard_sync?: boolean;
    /** whether to accept file transfer */
    accept_file_transfer?: boolean;
  };

  type SystemInfo = {
    /** List of CPU information */
    cpus: CpuInfo[];
    /** Host name */
    host_name?: any;
    /** System kernel version */
    kernel_version?: any;
    /** System name */
    name?: any;
    /** Operating system version */
    os_version?: any;
    /** Startup mode */
    startup_mode: string;
    /** Total memory in bytes */
    total_memory: number;
    /** Total swap in bytes */
    total_swap: number;
    /** Used memory in bytes */
    used_memory: number;
    /** Used swap in bytes */
    used_swap: number;
  };

  type SystemSettings = {
    /** Client ID for telemetry */
    client_id?: any;
    /** Enable IPv6 support */
    enable_ipv6?: boolean;
    /** listen ipv4 address for the server to bind to */
    listen_addr_ipv4?: string;
    /** listen ipv6 address for the server to bind to */
    listen_addr_ipv6?: string;
    /** Optional locale setting (e.g., "en", "zh-CN") */
    locale?: any;
    /** access logs are printed with the INFO level so ensure it is enabled by default */
    log_level?: string;
    /** Whether to open the browser automatically on server startup */
    open_browser_on_startup?: boolean;
    /** port number for the server to bind to */
    port?: number;
    /** Signaling server url, if not set, it will be "ws://127.0.0.1:{port}/signaling" */
    signaling_url?: any;
    /** Telemetry consent status */
    telemetry_consent?: any;
    /** Enable Rust backtrace for errors */
    traceback?: boolean;
  };

  type TelemetryConsent = {
    consent: boolean;
  };

  type TelemetryStatus = {
    consented?: any;
    needed: boolean;
  };

  type TerminalInputData = {
    content: string;
  };

  type TerminalList = {
    /** terminal command list */
    commands: string[][];
    /** current terminal index */
    current: number;
  };

  type TerminalOutputData = {
    content: string;
  };

  type TerminalResizeData = {
    cols: number;
    rows: number;
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

  type VersionInfo = {
    /** The version of the API. This is a simple integer that increments when API is changed. */
    api_version: number;
    /** The build number of the server. */
    build_number: number;
    /** The commit hash of the server. */
    commit_hash: string;
    /** Display name of the remote desk. */
    display_name?: any;
    /** Operation system associated with the version. */
    operation_system: OperationSystemEnum;
    /** Remote desk type associated with the version. */
    remote_desk_type: RemoteDeskTypeEnum;
  };

  type VpxEncoderSettings = {
    /** Bitrate in bps (bits per second), default is 5,000,000 bps (5 Mbps) */
    bps?: number;
  };
}
