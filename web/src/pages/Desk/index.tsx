import { querySettings } from "@/services/desk/querySettings";
import { updateSettings } from "@/services/desk/updateSettings";
import { FooterToolbar, PageContainer, ProForm, ProFormDateRangePicker, ProFormDigit, ProFormSelect, ProFormSwitch, ProFormText } from "@ant-design/pro-components";
import { useIntl, useModel } from "@umijs/max";
import { Alert, Button, Divider, FloatButton, message, Modal } from "antd";
import { useEffect, useRef, useState } from "react";

import styles from './index.less'; // 告诉 umi 编译这个 less
import { CommentOutlined, CustomerServiceOutlined } from "@ant-design/icons";

const SIGNALING_TYPE_CODE_INIT = 0;
const SIGNALING_TYPE_CODE_OFFER = 100;
const SIGNALING_TYPE_CODE_ANSWER = 101;
const SIGNALING_TYPE_CODE_CANID = 200;
const SIGNALING_TYPE_CODE_ERROR = 1000;
const SIGNALING_TYPE_CODE_UNKNOWN_TYPE = 1001;


const Desk: React.FC = () => {
  const { initialState, setInitialState } = useModel('@@initialState');
  const intl = useIntl();
  const remote_video = useRef<HTMLVideoElement>(null);
  const remote_audio = useRef<HTMLAudioElement>(null);
  const socketRef = useRef<WebSocket>();
  const peerconnectionRef = useRef<RTCPeerConnection>();

  const [dimensions, setDimensions] = useState({ width: 0, height: 0 });
  const [mousePosition, setMousePosition] = useState({ x: 0, y: 0 });
  const [isModalOpen, setIsModalOpen] = useState(true);
  //let socket = null;
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
        const signaling_model = JSON.parse(event.data) as API.SignalingModel;
        switch (signaling_model.signaling_type) {
          case SIGNALING_TYPE_CODE_INIT:
            const init_signaling_data = JSON.parse(signaling_model.signaling_data!) as API.InitSignalingData;

            let pc = new RTCPeerConnection({
              iceServers: init_signaling_data.ice_servers
            });
            pc.ontrack = function (event) {
              console.log("ontrack", event);
              switch (event.track.kind) {
                case 'video':
                  var video_ref = remote_video.current!;
                  video_ref.srcObject = event.streams[0];
                  video_ref.autoplay = true;
                  video_ref.controls = false;
                  video_ref.onresize = (event,) => { console.log("video on resize, width=" + video_ref.clientWidth + ", height=" + video_ref.clientHeight + ", event=", event) };
                  break;
                case 'audio':
                  var audio_ref = remote_audio.current!;
                  audio_ref.srcObject = event.streams[0];
                  audio_ref.autoplay = true;
                  audio_ref.controls = true;
                  break;
                default:
                  console.error("Unknown track kind", event.track.kind);
                  throw new Error("Unknown track kind");
              };
            };
            pc.oniceconnectionstatechange = e => {
              console.log("pc.iceConnectionState=" + pc.iceConnectionState + ", event is ", e);
            };
            pc.onicecandidate = event => {
              if (event.candidate === null) {
                const local_description_json = JSON.stringify(pc.localDescription);
                console.log("event.candidate === null, pc.localDescription=" + local_description_json)

                let offer_sginaling = {
                  signaling_type: SIGNALING_TYPE_CODE_OFFER,
                  signaling_data: local_description_json,
                } as API.SignalingModel;
                let offer_signaling_json = JSON.stringify(offer_sginaling);

                sock.send(offer_signaling_json);
              }
            };


            // Offer to receive 1 audio, and 1 video track
            pc.addTransceiver('video', { 'direction': 'sendrecv' });
            pc.addTransceiver('audio', { 'direction': 'sendrecv' });

            pc.createOffer().then(d => {
              pc.setLocalDescription(d);
              const local_description_json = JSON.stringify(d);
              console.info("create offer, local description=" + local_description_json);
            }).catch((reason) => { console.log("Create offer failed, reason=", reason) });
            peerconnectionRef.current = pc;
            break;
          case SIGNALING_TYPE_CODE_ANSWER:
            const answer_description_json = JSON.parse(signaling_model.signaling_data!) as RTCSessionDescriptionInit;
            console.info("set remote description answer_description_json=" + signaling_model.signaling_data);
            peerconnectionRef.current?.setRemoteDescription(new RTCSessionDescription(answer_description_json));
            break;

          default:
            break;
        }
      };
      sock.onerror = (event) => {
        console.error('WebSocket 错误:', event);
      };
      sock.onclose = (event) => {
        console.log('连接关闭', event);
      };


      socketRef.current = sock;

      const resizeObserver = new ResizeObserver(entries => {
        for (let entry of entries) {
          console.log("The size of video element changed: ", entry);
          setDimensions({
            width: entry.contentRect.width,
            height: entry.contentRect.height,
          });
        }
      });

      resizeObserver.observe(remote_video.current!);

      return () => {
        console.log("关闭websocket", sock);
        sock.close();
        console.log("关闭webrtc peer connection", peerconnectionRef.current);
        peerconnectionRef.current?.close();
        resizeObserver.disconnect();
      };
    })();
  }, []);


  const handleMouseMove = (event: MouseEvent) => {
    // Access mouse coordinates from the event object
    // console.log("Mouse move on video element: ", event);
    setMousePosition({
      x: event.clientX, // X-coordinate relative to the viewport
      y: event.clientY, // Y-coordinate relative to the viewport
    });
  };

  const showModal = () => {
    setIsModalOpen(true);
  };

  const handleOk = () => {
    setIsModalOpen(false);
  };

  const handleCancel = () => {
    setIsModalOpen(false);
  };

  return (
    <PageContainer>
      <video ref={remote_video} autoPlay muted className={styles.videoContainer} onMouseMove={handleMouseMove} />
      <audio ref={remote_audio} autoPlay />
      <Divider />

      <FloatButton.Group
        /*open={true}*/
        shape="square"
        trigger="hover"
        /*style={{ insetInlineEnd: 24 }}*/
        icon={<CustomerServiceOutlined />}
      >
        <FloatButton tooltip={<div>全屏</div>} />
        <FloatButton />
        <FloatButton icon={<CommentOutlined />} />
      </FloatButton.Group>

      <Modal
        title="Basic Modal"
        closable={{ 'aria-label': 'Custom Close Button' }}
        open={isModalOpen}
        footer={false}
      >

        <ProForm
          submitter={{
            render: (props, doms) => {
              return [...doms,
              <Button htmlType="button" onClick={handleCancel} key="close">
                关闭
              </Button>];
            },
          }}
        >
          <ProForm.Group>
            <ProFormSelect
              name="device"
              label="显示器"
              valueEnum={{
                open: '未解决',
                closed: '已解决',
              }}
              placeholder="请选择一个显示器"
              rules={[{ required: true, message: 'Please select your country!' }]}
            />
          </ProForm.Group>
          <ProForm.Group>
            <ProFormText
              name={['contract', 'name']}
              label="合同名称"
              placeholder="请输入名称"
            />
            <ProFormText
              name="name"
              label="签约客户名称"
              tooltip="最长为 24 位"
              placeholder="请输入名称"
            />
          </ProForm.Group>
        </ProForm>
      </Modal>
    </PageContainer>

  );
}

export default Desk;