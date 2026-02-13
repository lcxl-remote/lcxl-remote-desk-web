import { querySettings } from "@/services/desk/querySettings";
import { updateSettings } from "@/services/desk/updateSettings";
import { PageContainer, ProForm, ProFormDigit, ProFormSelect, ProFormSwitch, ProFormText } from "@ant-design/pro-components";
import { useIntl, useModel, useParams } from "@umijs/max";
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
  const { deskId } = useParams<{ deskId: string }>();
  const socketRef = useRef<WebSocket>();

  const [commandSelectOptions, setCommandSelectOptions] = useState<DefaultOptionType[] | undefined>();
  const [selectedCommand, setSelectedCommand] = useState<string | undefined>();
  const fitAddonRef = useRef<FitAddon>();


  const [terminal, setTerminal] = useState<Terminal>();

  const closeTerminal = () => {
    if (socketRef.current) {
      console.log(intl.formatMessage({ id: 'pages.deskTerminal.closeWebSocket' }), socketRef.current);
      socketRef.current.close();
      socketRef.current = undefined;
    }
    if (terminal) {
      console.log("关闭xterm");
      message.info(intl.formatMessage({ id: 'pages.deskTerminal.closeXterm' }));
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
    let wsUri = `${proto}://${location.host}/api/desk/terminal?command=${command}`;
    if (deskId) {
      wsUri += `&session_id=${deskId}`;
    }
    const sock = new WebSocket(wsUri);

    sock.onopen = (event) => {
      console.log('连接成功', event);
      message.info(intl.formatMessage({ id: 'pages.deskTerminal.connectSuccess' }));
    };

    socketRef.current = sock;

    const new_terminal = new Terminal({ cursorBlink: true, windowsMode: false });

    new_terminal.open(document.getElementById('terminal-container')!);

    const fitAddon = new FitAddon();
    // terminal 的尺寸与父元素匹配
    new_terminal.loadAddon(fitAddon);
    fitAddon.fit();
    fitAddonRef.current = fitAddon;

    // add web links addon to terminal
    new_terminal.loadAddon(new WebLinksAddon());

    new_terminal.writeln(intl.formatMessage({ id: 'pages.deskTerminal.welcomeMessage' }));
    setTerminal(new_terminal);

    // Custom WebSocket handling for JSON protocol
    sock.onmessage = (event) => {
      if (typeof event.data === 'string') {
        try {
          const msg = JSON.parse(event.data);
          if (msg.type === 'data') {
            new_terminal.write(msg.content);
          }
        } catch (e) {
          new_terminal.write(event.data);
        }
      } else {
        // Handle binary data (Blob or ArrayBuffer)
        const reader = new FileReader();
        reader.onload = () => {
          if (reader.result instanceof ArrayBuffer) {
            new_terminal.write(new Uint8Array(reader.result));
          }
        };
        reader.readAsArrayBuffer(event.data);
      }
    };

    sock.onclose = () => {
      console.log("WebSocket connection closed");
      new_terminal.write('\r\n\x1b[2mConnection closed.\x1b[0m');
      new_terminal.options.cursorBlink = false;
    };

    new_terminal.onData((data) => {
      if (sock.readyState === WebSocket.OPEN) {
        sock.send(JSON.stringify({ type: 'data', content: data }));
      }
    });

    new_terminal.onResize((size) => {
      if (sock.readyState === WebSocket.OPEN) {
        sock.send(JSON.stringify({ type: 'resize', rows: size.rows, cols: size.cols }));
      }
    });

    // Send initial size
    if (sock.readyState === WebSocket.OPEN) {
      sock.send(JSON.stringify({ type: 'resize', rows: new_terminal.rows, cols: new_terminal.cols }));
    } else {
      sock.addEventListener('open', () => {
        sock.send(JSON.stringify({ type: 'resize', rows: new_terminal.rows, cols: new_terminal.cols }));
      });
    }
  }

  const handleReloadTerminal = (e: React.MouseEvent<HTMLButtonElement>) => {
    reloadTerminal();


  }

  useEffect(() => {
    const container = document.getElementById('terminal-container');
    if (!container) return;

    let timeoutId: ReturnType<typeof setTimeout>;
    const resizeObserver = new ResizeObserver(() => {
      clearTimeout(timeoutId);
      timeoutId = setTimeout(() => {
        if (fitAddonRef.current) {
          fitAddonRef.current.fit();
        }
      }, 100); // Debounce for 100ms
    });

    resizeObserver.observe(container);

    return () => {
      resizeObserver.disconnect();
      clearTimeout(timeoutId);
    };
  }, []);

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