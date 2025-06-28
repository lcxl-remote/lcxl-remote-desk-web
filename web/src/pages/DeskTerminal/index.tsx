import { querySettings } from "@/services/desk/querySettings";
import { updateSettings } from "@/services/desk/updateSettings";
import { PageContainer, ProForm, ProFormDigit, ProFormSelect, ProFormSwitch, ProFormText } from "@ant-design/pro-components";
import { useIntl, useModel } from "@umijs/max";
import { Alert, Divider, message, Select } from "antd";
import { useEffect, useRef, useState } from "react";
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { AttachAddon } from "@xterm/addon-attach";

import '@xterm/xterm/css/xterm.css'
import { listTerminal } from "@/services/desk/listTerminal";
import { DefaultOptionType } from "antd/es/select";

const DeskTerminal: React.FC = () => {
  const { initialState, setInitialState } = useModel('@@initialState');
  const intl = useIntl();
  const socketRef = useRef<WebSocket>();
  const [commandSelectOptions, setCommandSelectOptions] = useState<DefaultOptionType[] | undefined>();

  const [terminal, setTerminal] = useState<Terminal | null>(null);

  const initTerminal = (sock: WebSocket) => {
    const terminal = new Terminal({ cursorBlink: true, windowsMode: true });

    terminal.open(document.getElementById('terminal-container')!);

    const fitAddon = new FitAddon();
    // terminal 的尺寸与父元素匹配
    terminal.loadAddon(fitAddon);
    fitAddon.fit();

    // add websocket addon to terminal
    const attachAddon = new AttachAddon(sock);
    terminal.loadAddon(attachAddon);

    terminal.writeln('\x1b[1;1;32mWelcome to LCXL Web Remote Desk Terminal!\x1b[0m');
    setTerminal(terminal);
  }

  //let socket = null;
  useEffect(() => {
    (async () => {
      const { location } = window;

      const response = await listTerminal();
      let select_options = response.commands.map((command: string[]) => {
        let option: DefaultOptionType = {
          label: command[0],
          value: JSON.stringify(command),
        };
        return option;
      });
      setCommandSelectOptions(select_options);
      //let command = encodeURIComponent("C:\\Windows\\System32\\cmd.exe");
      let command = encodeURIComponent("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
      const proto = location.protocol.startsWith('https') ? 'wss' : 'ws';
      const wsUri = `${proto}://${location.host}/api/desk/terminal?command=${command}`;
      const sock = new WebSocket(wsUri);

      sock.onopen = (event) => {
        console.log('连接成功', event);
      };

      socketRef.current = sock;

      initTerminal(sock);
      return () => {
        console.log("关闭websocket", sock);
        sock.close();
        console.log("关闭xterm");
        terminal?.dispose();
        setTerminal(null);
      };
    })();
  }, []);

  return (
    <PageContainer>
      <Select showSearch options={commandSelectOptions} style={{ width: '100%' }}/>
      <div id="terminal-container" style={{ width: "100%", height: "100%" }}></div>
    </PageContainer>

  );
}

export default DeskTerminal;