import { querySettings } from "@/services/desk/querySettings";
import { updateSettings } from "@/services/desk/updateSettings";
import { PageContainer, ProForm, ProFormDigit, ProFormSelect, ProFormSwitch, ProFormText } from "@ant-design/pro-components";
import { useIntl, useModel } from "@umijs/max";
import { Alert, Button, Divider, message, Select, Space } from "antd";
import { useEffect, useRef, useState } from "react";
import { listTerminal } from "@/services/desk/listTerminal";
import { DefaultOptionType } from "antd/es/select";

import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { AttachAddon } from "@xterm/addon-attach";
import { WebLinksAddon } from '@xterm/addon-web-links';

import '@xterm/xterm/css/xterm.css'
import { ReloadOutlined } from "@ant-design/icons";


const DeskTerminal: React.FC = () => {
  const { initialState, setInitialState } = useModel('@@initialState');
  const intl = useIntl();
  const socketRef = useRef<WebSocket>();

  const [commandSelectOptions, setCommandSelectOptions] = useState<DefaultOptionType[] | undefined>();
  const [selectedCommand, setSelectedCommand] = useState<string | undefined>();


  const [terminal, setTerminal] = useState<Terminal>();

  const closeTerminal = () => {
    if (socketRef.current) {
      console.log(intl.formatMessage({ id: 'pages.deskTerminal.closeWebSocket' }), socketRef.current);
      socketRef.current.close();
      socketRef.current = undefined;
    }
    if (terminal) {
      console.log(intl.formatMessage({ id: 'pages.deskTerminal.closeXterm' }));
      terminal?.dispose();
      setTerminal(undefined);
    }
  }

  const reloadTerminal = () => {
    closeTerminal();

    if (!selectedCommand) {

      return;
    }
    const select_command: string[] = JSON.parse(selectedCommand);
    const command = encodeURIComponent(select_command.join(","));
    // create new websocket connection
    const proto = location.protocol.startsWith('https') ? 'wss' : 'ws';
    const wsUri = `${proto}://${location.host}/api/desk/terminal?command=${command}`;
    const sock = new WebSocket(wsUri);

    sock.onopen = (event) => {
      console.log(intl.formatMessage({ id: 'pages.deskTerminal.connectSuccess' }), event);
    };

    socketRef.current = sock;

    const new_terminal = new Terminal({ cursorBlink: true, windowsMode: false });

    new_terminal.open(document.getElementById('terminal-container')!);

    const fitAddon = new FitAddon();
    // terminal 的尺寸与父元素匹配
    new_terminal.loadAddon(fitAddon);
    fitAddon.fit();

    // add websocket addon to terminal
    const attachAddon = new AttachAddon(sock);
    new_terminal.loadAddon(attachAddon);

    // add web links addon to terminal
    new_terminal.loadAddon(new WebLinksAddon());

    new_terminal.writeln(intl.formatMessage({ id: 'pages.deskTerminal.welcomeMessage' }));
    setTerminal(new_terminal);
  }

  const handleReloadTerminal = (e: React.MouseEvent<HTMLButtonElement>) => {
    reloadTerminal();


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

      return () => {
        closeTerminal();
      };
    })();
  }, []);

  return (
    <PageContainer>
      <div>

        <Space.Compact block>
          <Select showSearch options={commandSelectOptions} style={{ width: '100%' }} value={selectedCommand} onChange={setSelectedCommand} />
          <Button type="primary" icon={<ReloadOutlined />} onClick={handleReloadTerminal} disabled={!selectedCommand}>{intl.formatMessage({ id: 'pages.deskTerminal.reload' })}</Button>
        </Space.Compact>

      </div>
      <div id="terminal-container" style={{ width: "100%", height: "100%" }}></div>
    </PageContainer>
  );
}
    if (terminal) {
      console.log("关闭xterm");
      terminal?.dispose();
      setTerminal(undefined);
    }
  }

  const reloadTerminal = () => {
    closeTerminal();

    if (!selectedCommand) {

      return;
    }
    const select_command: string[] = JSON.parse(selectedCommand);
    const command = encodeURIComponent(select_command.join(","));
    // create new websocket connection
    const proto = location.protocol.startsWith('https') ? 'wss' : 'ws';
    const wsUri = `${proto}://${location.host}/api/desk/terminal?command=${command}`;
    const sock = new WebSocket(wsUri);

    sock.onopen = (event) => {
      console.log('连接成功', event);
    };

    socketRef.current = sock;

    const new_terminal = new Terminal({ cursorBlink: true, windowsMode: false });

    new_terminal.open(document.getElementById('terminal-container')!);

    const fitAddon = new FitAddon();
    // terminal 的尺寸与父元素匹配
    new_terminal.loadAddon(fitAddon);
    fitAddon.fit();

    // add websocket addon to terminal
    const attachAddon = new AttachAddon(sock);
    new_terminal.loadAddon(attachAddon);

    // add web links addon to terminal
    new_terminal.loadAddon(new WebLinksAddon());

    new_terminal.writeln('\x1b[1;1;32mWelcome to LCXL Web Remote Desk Terminal!\x1b[0m');
    setTerminal(new_terminal);
  }

  const handleReloadTerminal = (e: React.MouseEvent<HTMLButtonElement>) => {
    reloadTerminal();


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

      return () => {
        closeTerminal();
      };
    })();
  }, []);

  return (
    <PageContainer>
      <div>

        <Space.Compact block>
          <Select showSearch options={commandSelectOptions} style={{ width: '100%' }} value={selectedCommand} onChange={setSelectedCommand} />
          <Button type="primary" icon={<ReloadOutlined />} onClick={handleReloadTerminal} disabled={!selectedCommand}>重新加载</Button>
        </Space.Compact>

      </div>
      <div id="terminal-container" style={{ width: "100%", height: "100%" }}></div>
    </PageContainer>

  );
}

export default DeskTerminal;