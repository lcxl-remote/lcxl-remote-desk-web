import { querySettings } from "@/services/desk/querySettings";
import { updateSettings } from "@/services/desk/updateSettings";
import { PageContainer, ProForm, ProFormDigit, ProFormSelect, ProFormSwitch, ProFormText } from "@ant-design/pro-components";
import { useIntl, useModel } from "@umijs/max";
import { Alert, Divider, message } from "antd";
import { useEffect, useRef, useState } from "react";



const Desk: React.FC = () => {
  const { initialState, setInitialState } = useModel('@@initialState');
  const intl = useIntl();

  //const [socket, setSocket] = useState();
  let socket = null;
  useEffect(() => {
    (async () => {
      const { location } = window;

      const proto = location.protocol.startsWith('https') ? 'wss' : 'ws';
      const wsUri = `${proto}://${location.host}/api/desk/signaling`;
      const sock = new WebSocket(wsUri);

      sock.onopen = (event) => {
        console.log('连接成功', event);
      };
      sock.onmessage = (event) => {
        console.log('收到消息:', event.data);
      };
      sock.onerror = (event) => {
        console.error('WebSocket 错误:', event);
      };
      sock.onclose = (event) => {
        console.log('连接关闭', event);
      };


      //setSocket(sock);
      socket = sock;
      return () => {
        sock.close();
      };
    })();
  }, []);


  return (
    <PageContainer>
      <Divider />

    </PageContainer>

  );
}

export default Desk;