import { querySettings } from "@/services/desk/querySettings";
import { updateSettings } from "@/services/desk/updateSettings";
import { FooterToolbar, PageContainer, ProForm, ProFormDateRangePicker, ProFormDigit, ProFormInstance, ProFormSelect, ProFormSlider, ProFormSwitch, ProFormText, ProFormTreeSelect, RequestOptionsType } from "@ant-design/pro-components";
import { useIntl, useModel } from "@umijs/max";
import { Alert, Button, Divider, Flex, FloatButton, message, Modal } from "antd";
import { useCallback, useEffect, useRef, useState } from "react";

import styles from './index.less'; // 告诉 umi 编译这个 less
import { CommentOutlined, CustomerServiceOutlined, FullscreenOutlined, SettingOutlined } from "@ant-design/icons";

const SIGNALING_TYPE_CODE_INIT = 0;
const SIGNALING_TYPE_CODE_OFFER = 100;
const SIGNALING_TYPE_CODE_ANSWER = 101;
const SIGNALING_TYPE_CODE_CANID = 102;

const SIGNALING_TYPE_CODE_REQUIRE_CONTROL = 201;
const SIGNALING_TYPE_CODE_ACCEPT_CONTROL = 202;
const SIGNALING_TYPE_CODE_DENY_CONTROL = 203;

const SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS = 301;

const SIGNALING_TYPE_CODE_ERROR = 1000;
const SIGNALING_TYPE_CODE_UNKNOWN_TYPE = 1001;

type OfferModel = {
  offer: RTCSessionDescription,
  desk_settings: API.DeskSettings,
};

type DeskFormValues = API.DeskSettings & {
  enable_audio?: boolean,
}
const Desk: React.FC = () => {
  const { initialState, setInitialState } = useModel('@@initialState');
  const intl = useIntl();
  const remote_video = useRef<HTMLVideoElement>(null);
  const remote_audio = useRef<HTMLAudioElement>(null);
  const socketRef = useRef<WebSocket>();
  const peerconnectionRef = useRef<RTCPeerConnection>();

  const [formInstance, setFormInstance] = useState<ProFormInstance<DeskFormValues>>();
  const [dimensions, setDimensions] = useState({ width: 0, height: 0 });
  const [mousePosition, setMousePosition] = useState({ x: 0, y: 0 });
  const [isModalOpen, setIsModalOpen] = useState(false);
  const [initSignalingData, setInitSignalingData] = useState<API.InitSignalingData>();

  const formRef = useCallback(node => {
    const _formInstance = node as ProFormInstance<DeskFormValues>;
    if (_formInstance !== null) {
      if (formInstance == null) {
        setFormInstance(_formInstance);
      }
      if (initSignalingData != null) {
        _formInstance.setFieldsValue(
          {
            enable_audio: initSignalingData.desk_settings.audio_device != null,
            ...initSignalingData.desk_settings
          }
        );
      }
    }
  }, [initSignalingData]);
  //const formRef = useRef<ProFormInstance>();

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

  const sendSignalingMessage = (signaling_type: number, signaling_data: any) => {
    const sock = socketRef.current!;
    let sginaling = {
      signaling_type,
      signaling_data: JSON.stringify(signaling_data),
    } as API.SignalingModel;
    let signaling_json = JSON.stringify(sginaling);

    sock.send(signaling_json);
  }
  /**
   * Handle WebSocket message event
   * @param event - WebSocket message event
   */
  const handleWebSocketMessage = (event: MessageEvent) => {
    const sock = socketRef.current!;
    console.log('收到消息:', event.data);

    const signaling_model = JSON.parse(event.data) as API.SignalingModel;
    switch (signaling_model.signaling_type) {
      case SIGNALING_TYPE_CODE_INIT:
        const init_signaling_data = JSON.parse(signaling_model.signaling_data!) as API.InitSignalingData;
        console.log('初始化信令数据:', init_signaling_data);
        setInitSignalingData(init_signaling_data);
        setIsModalOpen(true);
        break;
      case SIGNALING_TYPE_CODE_ANSWER:
        const answer_description_json = JSON.parse(signaling_model.signaling_data!) as RTCSessionDescriptionInit;
        console.info("set remote description answer_description_json=" + signaling_model.signaling_data);
        peerconnectionRef.current?.setRemoteDescription(new RTCSessionDescription(answer_description_json));
        break;

      default:
        break;
    }
  }
  //let socket = null;
  useEffect(() => {
    const { location } = window;

    const proto = location.protocol.startsWith('https') ? 'wss' : 'ws';
    const wsUri = `${proto}://${location.host}/api/desk/signaling`;
    const sock = new WebSocket(wsUri);
    socketRef.current = sock;

    sock.onopen = (event) => {
      console.log('连接成功', event);
    };
    sock.onmessage = handleWebSocketMessage;
    sock.onerror = (event) => {
      console.error('WebSocket 错误:', event);
    };
    sock.onclose = (event) => {
      console.log('连接关闭', event);
    };

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

  const handleOk = async (formData: DeskFormValues) => {
    console.log(JSON.stringify(formData));
    if (!formData.enable_audio) {
      formData.audio_device = null;
    }
    delete formData.enable_audio;
    console.log(JSON.stringify(formData));
    const desk_settings = formData
    if (!peerconnectionRef.current) {
      // 创建一个新的RTCPeerConnection实例
      peerconnectionRef.current = createRTCPeerConnection(desk_settings);
    } else {
      // 更新配置
      sendSignalingMessage(SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS, desk_settings);
    }
    setIsModalOpen(false);
    return;
  };

  const createRTCPeerConnection = (desk_settings: API.DeskSettings) => {
    let pc = new RTCPeerConnection({
      iceServers: initSignalingData!.ice_servers
    });
    pc.ontrack = function (event) {
      console.log("ontrack", event);
      console.log("get track setting: ", event.track.getSettings());
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
          desk_settings,
        } as OfferModel;

        console.log("event.candidate === null, offer_model: ", offer_model)

        sendSignalingMessage(SIGNALING_TYPE_CODE_OFFER, offer_model);
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

    return pc;
  }

  const handleCancel = () => {
    setIsModalOpen(false);
  };

  const image_capture_list = initSignalingData != null ? Object.keys(initSignalingData.video_device_list) : undefined;
  const imageCaptureSelectMap = image_capture_list?.reduce((map, item) => {
    map.set(item, item);
    return map;
  }, new Map<string, string>);


  const audio_capture_list = initSignalingData != null ? Object.keys(initSignalingData.audio_device_list) : undefined;
  const audioCaptureSelectMap = audio_capture_list?.reduce((map, item) => {
    map.set(item, item);
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

        <ProForm<DeskFormValues>
          formRef={formRef}
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
          <Divider plain>显示配置</Divider>
          <ProForm.Group>
            <ProFormSelect
              name="image_capture"
              label="屏幕捕获模式"
              valueEnum={imageCaptureSelectMap}
              placeholder="请选择一个屏幕捕获模式"
              rules={[{ required: true, message: '请选择一个屏幕捕获模式!' }]}

            />
          </ProForm.Group>
          <ProForm.Group>
            <ProForm.Item noStyle shouldUpdate>
              {(form) => {
                const imageCapture = form.getFieldValue('image_capture');

                const videoDeviceSelectMap = initSignalingData?.video_device_list[imageCapture]?.reduce((map, item, currentIndex) => {
                  map.set(currentIndex, `${item.display_device_name} (${item.desktop_coordinates.right}x${item.desktop_coordinates.bottom})`);
                  return map;
                }, new Map<number, string>);
                return (
                  <ProFormSelect
                    name="video_device_index"
                    label="显示设备"
                    valueEnum={videoDeviceSelectMap}
                    placeholder="请选择一个显示设备"
                    rules={[{ required: true, message: '请选择一个显示设备!' }]}
                    colProps={{
                      span: 16,
                    }}
                    disabled={!imageCapture}
                  />)
              }}</ProForm.Item>

            <ProFormSwitch name="show_mouse" label="显示远程鼠标" colProps={{
              span: 8,
            }} />
          </ProForm.Group>

          <ProForm.Group>
            <ProFormSwitch name="adaptive_web_page_resolution" label="自适应网页分辨率" colProps={{
              span: 8,
            }} />
            <ProForm.Item noStyle shouldUpdate>
              {(form) => {
                return (<ProFormSlider
                  name="video_zoom_ratio"
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
                  disabled={form.getFieldValue("adaptive_web_page_resolution")}
                />)
              }}
            </ProForm.Item>

          </ProForm.Group>
          <Divider plain>音频配置</Divider>

          <ProForm.Group>
            <ProFormSwitch name="enable_audio" label="捕获音频" colProps={{
              span: 4,
            }} />
            <ProForm.Item noStyle shouldUpdate>
              {(form) => {



                return (
                  <ProFormSelect
                    name="audio_capture"
                    label="音频捕获模式"
                    valueEnum={audioCaptureSelectMap}
                    placeholder="请选择一个音频捕获模式"
                    rules={[{ required: form.getFieldValue("enable_audio"), message: '请选择一个音频捕获模式!' }]}
                    disabled={!form.getFieldValue("enable_audio")}
                    colProps={{
                      span: 20,
                    }}
                  />);
              }
              }</ProForm.Item>
          </ProForm.Group>

          <ProForm.Group>

            {/* noStyle shouldUpdate 是必选的，写了 name 就会失效 */}
            <ProForm.Item noStyle shouldUpdate>
              {(form) => {

                const audioCapture = form.getFieldValue('audio_capture');
                const audioDeviceSelectMap = initSignalingData?.audio_device_list[audioCapture]?.reduce((map, item) => {
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

                return (<ProFormSelect
                  name="audio_device"
                  label="音频设备"
                  valueEnum={audioDeviceSelectMap}
                  placeholder="请选择一个音频设备"
                  rules={[{ required: form.getFieldValue("enable_audio"), message: '请选择一个音频设备!' }]}

                  disabled={!form.getFieldValue("enable_audio") || !audioDeviceSelectMap}
                  convertValue={(value, namePath) => {
                    let result = value;
                    if (typeof value != "string") {
                      result = JSON.stringify(value)
                    }
                    return result;
                  }}
                  transform={(value, namePath, allValues) => {
                    let result = value;
                    if (typeof value == "string") {
                      result = JSON.parse(value);
                    }
                    return { audio_device: result };
                  }}
                />)
              }}
            </ProForm.Item>
          </ProForm.Group>
          <Divider plain>编码配置</Divider>
          <ProForm.Group>
            <ProFormSelect
              name="video_encoder"
              label="视频编码"
              valueEnum={videoEncodeTypeSelectMap}
              placeholder="自动检测"
              colProps={{
                span: 8,
              }}
            />
            <ProFormSwitch name="switch" label="自适应码率" colProps={{
              span: 8,
            }} />
            <ProFormDigit
              label="码率 bps"
              name="video_encode_bps"
              min={1000}
              max={1000_000_000_000}
              fieldProps={{ precision: 0 }}
              colProps={{
                span: 8,
              }}
            />
          </ProForm.Group>
          <ProForm.Group>
            <ProFormSelect
              name="audio_encoder"
              label="音频编码"
              valueEnum={audioEncodeTypeSelectMap}
              placeholder="自动检测"
            />
          </ProForm.Group>
          <Divider plain>高级</Divider>
          <ProForm.Group>
            <ProFormSwitch name="enable_d3d_debug" label="开启D3D调试" colProps={{
              span: 8,
            }} />
          </ProForm.Group>
        </ProForm>
      </Modal>
    </PageContainer>

  );
}

export default Desk;