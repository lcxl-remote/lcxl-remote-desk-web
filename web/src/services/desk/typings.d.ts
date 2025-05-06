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

  type deleteTurnSessionParams = {
    address: string;
    interface: string;
  };

  type FakeCaptcha = {
    code?: number;
    status?: any;
  };

  type FakeCaptchaParams = {
    phone?: any;
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

  type LabelKey = {
    key?: any;
    label?: any;
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
