import { querySettings } from "@/services/desk/querySettings";
import { updateSettings } from "@/services/desk/updateSettings";
import { FooterToolbar, PageContainer, ProForm, ProFormDateRangePicker, ProFormDigit, ProFormSelect, ProFormSlider, ProFormSwitch, ProFormText, ProFormTreeSelect, RequestOptionsType } from "@ant-design/pro-components";
import { useIntl, useModel } from "@umijs/max";
import { Alert, Button, Divider, Flex, FloatButton, message, Modal } from "antd";
import { useEffect, useRef, useState } from "react";

import styles from './index.less'; // 告诉 umi 编译这个 less
import { CommentOutlined, CustomerServiceOutlined, FullscreenOutlined, SettingOutlined } from "@ant-design/icons";

const SIGNALING_TYPE_CODE_INIT = 0;
const SIGNALING_TYPE_CODE_OFFER = 100;
const SIGNALING_TYPE_CODE_ANSWER = 101;
const SIGNALING_TYPE_CODE_CANID = 200;
const SIGNALING_TYPE_CODE_ERROR = 1000;
const SIGNALING_TYPE_CODE_UNKNOWN_TYPE = 1001;

type OfferModel = {
  offer: RTCSessionDescription,
  desk_config: API.DeskConfig,
};

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
  const [initSignalingData, setInitSignalingData] = useState<API.InitSignalingData>();

  const handleRemoteVideoResize = (event: UIEvent) => {
    var video_ref = remote_video.current!;
    console.log("video on resize, width=" + video_ref.clientWidth + ", height=" + video_ref.clientHeight + ", event=", event);
  };

  const handleRemoteVideoMouseMove = (event: MouseEvent) => {
    // Access mouse coordinates from the event object
    // console.log("Mouse move on video element: ", event);
    setMousePosition({
      x: event.clientX, // X-coordinate relative to the viewport
      y: event.clientY, // Y-coordinate relative to the viewport
    });
  };

  const handleRemoteVideoKeyDown = (event: KeyboardEvent) => {
    console.log("key down, event=", event);
  };

  const handleRemoteVideoKeyUp = (event: KeyboardEvent) => {
    console.log("key up, event=", event);
  };

  const handleRemoteVideoWaiting = (event: Event) => {
    console.log("video waiting, event=", event);
  }

  const addRemoteVideoEvent = (element: HTMLVideoElement) => {
    console.log("addRemoteVideoEvent");
    // Add resize event listener to the video element
    element.addEventListener("resize", handleRemoteVideoResize);
    // Add mouse move event listener to the video element
    element.addEventListener("mousemove", handleRemoteVideoMouseMove);

    element.addEventListener("keydown", handleRemoteVideoKeyDown);

    element.addEventListener("keyup", handleRemoteVideoKeyUp);

    element.addEventListener("waiting", handleRemoteVideoWaiting);

  };


  //let socket = null;
  useEffect(() => {
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
          setInitSignalingData(init_signaling_data);

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
                addRemoteVideoEvent(video_ref);
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
              const offer_model = {
                offer: pc.localDescription!,
                desk_config: {
                  video_device_index: 0,
                  // Video encode bitrate in bps: 10 Mbps
                  video_encode_bps: 10_000_000,
                  audio_device: {
                    audio_data_flow: "Render",
                    audio_device_id: null
                  }
                }
              } as OfferModel;

              console.log("event.candidate === null, offer_model: ", offer_model)

              let offer_sginaling = {
                signaling_type: SIGNALING_TYPE_CODE_OFFER,
                signaling_data: JSON.stringify(offer_model),
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
  }, []);




  const showModal = () => {
    setIsModalOpen(true);
  };

  const handleOk = async (formData) => {
    console.log(formData);
    setIsModalOpen(false);
    return;
  };

  const handleCancel = () => {
    setIsModalOpen(false);
  };

  const videoDeviceSelectMap = initSignalingData?.video_device_list.reduce((map, item) => {
    map.set(item.device_name, `${item.display_device_name} (${item.desktop_coordinates.right}x${item.desktop_coordinates.bottom})`);
    return map;
  }, new Map<string, string>);

  const audioDeviceSelectMap = initSignalingData?.audio_device_list.reduce((map, item) => {
    const default_audio_device = {
      audio_data_flow: item.data_flow,
      audio_device_id: null,
    } as API.SelectedAudioDevice;
    const default_audio_device_json_str = JSON.stringify(default_audio_device);
    let found_value = map.get(default_audio_device_json_str);
    if (!found_value) {
      map.set(default_audio_device_json_str, `[${item.data_flow}]默认设备`);
    }
    const audio_device = {
      audio_data_flow: item.data_flow,
      audio_device_id: item.id,
    } as API.SelectedAudioDevice;
    map.set(JSON.stringify(audio_device), `[${item.data_flow}]${item.firendly_name}${item.default ? "(当前默认)" : ""}`);
    return map;
  }, new Map<string, string>);

  const videoEncodeTypeSelectMap = {
    "h264": "H.264",
  };

  const audioEncodeTypeSelectMap = {
    "opus": "Opus",
  };

  return (
    <PageContainer>
      <video ref={remote_video} autoPlay muted className={styles.videoContainer} />
      <audio ref={remote_audio} autoPlay />
      <Divider />

      <FloatButton.Group
        /*open={true}*/
        shape="square"
        trigger="hover"
        /*style={{ insetInlineEnd: 24 }}*/
        icon={<CustomerServiceOutlined />}
      >
        <FloatButton tooltip={<div>全屏</div>} icon={<FullscreenOutlined />} />
        <FloatButton icon={<SettingOutlined />} onClick={showModal} />
        <FloatButton icon={<CommentOutlined />} />
      </FloatButton.Group>

      <Modal
        title="桌面配置"
        closable={{ 'aria-label': 'Custom Close Button' }}
        open={isModalOpen}
        footer={false}
      >

        <ProForm
          grid={true}
          submitter={{
            render: (props, doms) => {
              return <div><Divider /><Flex justify="flex-end" align="center" gap="small">
                {doms}
                <Button htmlType="button" onClick={handleCancel} key="close">
                  关闭
                </Button>
              </Flex></div>;
            },
          }}
          onFinish={handleOk}
        >
          <ProForm.Group>
            <ProFormSelect
              name="device"
              label="显示设备"
              valueEnum={videoDeviceSelectMap}
              placeholder="请选择一个显示设备"
              rules={[{ required: true, message: '请选择一个显示设备!' }]}
            />
          </ProForm.Group>
          <ProForm.Group>
            <ProFormSwitch name="switch" label="捕获音频" colProps={{
              span: 4,
            }} />
            {/* noStyle shouldUpdate 是必选的，写了 name 就会失效 */}
            <ProForm.Item noStyle shouldUpdate>
              {(form) => {
                return (<ProFormSelect
                  name="audio"
                  label="音频设备"
                  valueEnum={audioDeviceSelectMap}
                  placeholder="请选择一个音频设备"
                  rules={[{ required: form.getFieldValue("switch"), message: '请选择一个音频设备!' }]}
                  colProps={{
                    span: 20,
                  }}
                  disabled={!form.getFieldValue("switch")}
                />)
              }}
            </ProForm.Item>
          </ProForm.Group>
          <Divider />
          <ProForm.Group>
            <ProFormSwitch name="switch" label="自适应网页分辨率" colProps={{
              span: 8,
            }} />
            <ProFormSlider
              name="slider"
              label="远程分辨率缩放"
              min={10}
              marks={{
                25: '25%',
                50: '50%',
                75: '75%',
                100: '100%',
              }}
              colProps={{
                span: 16,
              }}
            />
          </ProForm.Group>
          <Divider />
          <ProForm.Group>
            <ProFormSelect
              name="video_encode_type"
              label="视频编码"
              valueEnum={videoEncodeTypeSelectMap}
              placeholder="请选择一个视频编码"
              rules={[{ required: true, message: '请选择一个视频编码' }]}
              colProps={{
                span: 8,
              }}
            />
            <ProFormSwitch name="switch" label="自适应码率" colProps={{
              span: 8,
            }} />
            <ProFormDigit
              label="码率Mbps"
              name="ideo_encode_mbps"
              min={1}
              max={1000}
              fieldProps={{ precision: 0 }}
              colProps={{
                span: 8,
              }}
            />
          </ProForm.Group>
          <ProForm.Group>
            <ProFormSelect
              name="audio_encode_type"
              label="音频编码"
              valueEnum={audioEncodeTypeSelectMap}
              placeholder="请选择一个音频编码"
              rules={[{ required: true, message: '请选择一个音频编码' }]}
            />
          </ProForm.Group>

        </ProForm>
      </Modal>
    </PageContainer>

  );
}

export default Desk;