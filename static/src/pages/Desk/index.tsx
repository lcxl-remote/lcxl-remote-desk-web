import { querySettings } from "@/services/desk/querySettings";
import { updateSettings } from "@/services/desk/updateSettings";
import { FooterToolbar, PageContainer, ProForm, ProFormDateRangePicker, ProFormDigit, ProFormInstance, ProFormSelect, ProFormSlider, ProFormSwitch, ProFormText, ProFormTreeSelect, RequestOptionsType } from "@ant-design/pro-components";
import { useIntl, useModel } from "@umijs/max";
import { Alert, Button, Divider, Flex, FloatButton, message, Modal, Tooltip } from "antd";
import { useCallback, useEffect, useRef, useState } from "react";

import styles from './index.less'; // 告诉 umi 编译这个 less
import { CommentOutlined, CustomerServiceOutlined, FullscreenExitOutlined, FullscreenOutlined, SettingOutlined, StopOutlined } from "@ant-design/icons";
import e from "express";

const SIGNALING_TYPE_CODE_INIT = 101;
const SIGNALING_TYPE_CODE_OFFER = 102;
const SIGNALING_TYPE_CODE_ANSWER = 103;
const SIGNALING_TYPE_CODE_CANID = 104;

const SIGNALING_TYPE_CODE_REQUIRE_CONTROL = 201;
const SIGNALING_TYPE_CODE_ACCEPT_CONTROL = 202;
const SIGNALING_TYPE_CODE_DENY_CONTROL = 203;
const SIGNALING_TYPE_CODE_CLOSE_CONTROL = 204;

const SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS = 301;

const SIGNALING_TYPE_CODE_ERROR = 10000000;
const SIGNALING_TYPE_CODE_UNKNOWN_TYPE = 10000001;

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
  const remoteVideo = useRef<HTMLVideoElement>(null);
  const remoteAudio = useRef<HTMLAudioElement>(null);
  const socketRef = useRef<WebSocket>();
  const peerconnectionRef = useRef<RTCPeerConnection>();
  const acceptControlRef = useRef(false);
  const dimensionsRef = useRef({ width: 0, height: 0 });
  // data channel
  const mouseEventDataChannelRef = useRef<RTCDataChannel>();
  const keyboardEventDataChannelRef = useRef<RTCDataChannel>();
  // const clipboardEventDataChannelRef = useRef<RTCDataChannel>();
  // const fileTransferEventDataChannelRef = useRef<RTCDataChannel>();

  // use state
  const [formInstance, setFormInstance] = useState<ProFormInstance<DeskFormValues>>();

  const [isModalOpen, setIsModalOpen] = useState(false);
  const [initSignalingData, setInitSignalingData] = useState<API.InitSignalingData>();
  const [acceptControl, setAcceptControl] = useState(false);
  const [isFullscreen, setIsFullscreen] = useState(false);
  const [showControls, setShowControls] = useState(false);

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
    var videoRef = remoteVideo.current!;
    console.log("video on resize, width=" + videoRef.clientWidth + ", height=" + videoRef.clientHeight + ", event=", event);
  };

  const handleMouseEvent = (eventType: string, event: MouseEvent) => {
    if (!acceptControlRef.current || !mouseEventDataChannelRef.current || mouseEventDataChannelRef.current.readyState !== "open") {
      return;
    }
    const dimensions = dimensionsRef.current;
    if (dimensions.width == 0 || dimensions.height == 0) {
      message.warning("视频尺寸未初始化，无法发送鼠标事件");
      return;
    }
    const x_ratio = event.offsetX / dimensions.width;
    const y_ratio = event.offsetY / dimensions.height;
    let delta_x = 0;
    let delta_y = 0;
    if (eventType == "wheel") {
      const wheelEvent = event as WheelEvent;
      delta_x = wheelEvent.deltaX;
      delta_y = wheelEvent.deltaY;
    }
    const mouseEvent = {
      event: eventType,
      x: x_ratio,
      y: y_ratio,
      button: event.button,
      buttons: event.buttons,
      alt_key: event.altKey,
      delta_x: delta_x,
      delta_y: delta_y,
    } as API.MouseEventData;
    const mouseEventJson = JSON.stringify(mouseEvent);
    if (eventType != "mousemove") {
      console.log("send mouse event: ", mouseEventJson);
    }
    mouseEventDataChannelRef.current.send(mouseEventJson);
  }
  const handleRemoteVideoMouseMove = (event: MouseEvent) => {
    handleMouseEvent("mousemove", event);
  };

  const handleKeyboardEvent = (eventType: string, event: KeyboardEvent) => {
    if (!acceptControlRef.current || !keyboardEventDataChannelRef.current || keyboardEventDataChannelRef.current.readyState !== "open") {
      return;
    }
    const keyboardEvent = {
      event: eventType,
      key: event.key,
      code: event.code,
      key_code: event.keyCode,
      alt_key: event.altKey,
      ctrl_key: event.ctrlKey,
      shift_key: event.shiftKey,
      meta_key: event.metaKey,
      repeat: event.repeat,
      location: event.location,
      is_composing: event.isComposing,
    } as API.KeyboardEventData;
    const keyboardEventJson = JSON.stringify(keyboardEvent);
    console.log("send keyboard event: ", keyboardEventJson);
    keyboardEventDataChannelRef.current.send(keyboardEventJson);
  }

  const handleRemoteVideoKeyDown = (event: KeyboardEvent) => {
    console.log("key down, event=", event);
    event.preventDefault();
    remoteVideo.current?.focus();
    handleKeyboardEvent("keydown", event);
  };

  const handleRemoteVideoKeyUp = (event: KeyboardEvent) => {
    console.log("key up, event=", event);
    event.preventDefault();
    remoteVideo.current?.focus();
    handleKeyboardEvent("keyup", event);
  };

  const handleRemoteVideoWaiting = (event: Event) => {
    console.log("video waiting, event=", event);
  }

  const handleRemoteVideoClick = (event: MouseEvent) => {
    console.log("video click, event=", event);
  }

  const handleRemoteVideoDblClick = (event: MouseEvent) => {
    console.log("video double click, event=", event);
  }

  const handleRemoteVideoMouseUp = (event: MouseEvent) => {
    event.preventDefault();
    remoteVideo.current?.focus();
    handleMouseEvent("mouseup", event);
    console.log("mouse up, event=", event);
  };

  const handleRemoteVideoMouseDown = (event: MouseEvent) => {
    event.preventDefault();
    remoteVideo.current?.focus();
    handleMouseEvent("mousedown", event);
    console.log("mouse down, event=", event);
  };

  const handleRemoteVideoContextmenu = (event: MouseEvent) => {
    // disable right click context menu
    event.preventDefault();
    remoteVideo.current?.focus();
    console.log("context menu, event=", event);
  }

  const handleRemoteVideoMousWheel = (event: WheelEvent) => {
    event.preventDefault();
    event.stopPropagation();
    remoteVideo.current?.focus();
    handleMouseEvent("wheel", event);
    console.log("mouse wheel, event=", event);
  };

  const addRemoteVideoEvent = (element: HTMLVideoElement) => {
    console.log("addRemoteVideoEvent");
    // Add resize event listener to the video element
    element.addEventListener("resize", handleRemoteVideoResize);
    // Add mouse move event listener to the video element
    element.addEventListener("mousemove", handleRemoteVideoMouseMove);

    element.addEventListener("mouseup", handleRemoteVideoMouseUp);

    element.addEventListener("mousedown", handleRemoteVideoMouseDown);

    element.addEventListener("click", handleRemoteVideoClick);

    element.addEventListener("dblclick", handleRemoteVideoDblClick);

    element.addEventListener("wheel", handleRemoteVideoMousWheel, { passive: false });

    element.addEventListener("keydown", handleRemoteVideoKeyDown);

    element.addEventListener("keyup", handleRemoteVideoKeyUp);

    element.addEventListener("waiting", handleRemoteVideoWaiting);

    element.addEventListener("contextmenu", handleRemoteVideoContextmenu);
  };

  const sendSignalingMessage = (signalingType: number, signalingData: any) => {
    const sock = socketRef.current!;
    let sginaling = {
      signaling_type: signalingType,
      signaling_data: signalingData,
    } as API.SignalingModel;
    let signalingJson = JSON.stringify(sginaling);

    sock.send(signalingJson);
  }
  /**
   * Handle WebSocket message event
   * @param event - WebSocket message event
   */
  const handleWebSocketMessage = (event: MessageEvent) => {
    const sock = socketRef.current!;
    console.log('收到消息:', event.data);

    const signalingModel = JSON.parse(event.data) as API.SignalingModel;
    switch (signalingModel.signaling_type) {
      case SIGNALING_TYPE_CODE_INIT:
        const initSignalingData = signalingModel.signaling_data as API.InitSignalingData;
        console.log('初始化信令数据:', initSignalingData);
        setInitSignalingData(initSignalingData);
        setIsModalOpen(true);
        break;
      case SIGNALING_TYPE_CODE_ANSWER:
        const answerDescriptionJson = signalingModel.signaling_data as RTCSessionDescriptionInit;
        console.info("set remote description answer_description_json=" + signalingModel.signaling_data);
        peerconnectionRef.current?.setRemoteDescription(new RTCSessionDescription(answerDescriptionJson));
        break;
      case SIGNALING_TYPE_CODE_ACCEPT_CONTROL:
        message.success("控制请求被接受，准备初始化控制通道");
        setAcceptControl(true);
        acceptControlRef.current = true;
        break;
      case SIGNALING_TYPE_CODE_DENY_CONTROL:
        message.error("控制请求被拒绝");
        break;
      case SIGNALING_TYPE_CODE_CLOSE_CONTROL:
        message.info("控制已被远程主机关闭");
        setAcceptControl(false);
        acceptControlRef.current = false;
        break;
      case SIGNALING_TYPE_CODE_ERROR:
        const error_message = signalingModel.signaling_data;
        message.error(`服务器错误: ${error_message}`);
        break;
      default:
        console.error("未知的 signaling_type: ", signalingModel.signaling_type);
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
        dimensionsRef.current = {
          width: entry.contentRect.width,
          height: entry.contentRect.height,
        };
      }
    });

    resizeObserver.observe(remoteVideo.current!);

    const handleFullscreenChange = () => {
      setIsFullscreen(!!document.fullscreenElement);
    };

    document.addEventListener('fullscreenchange', handleFullscreenChange);

    return () => {
      console.log("关闭websocket", sock);
      sock.close();
      console.log("关闭webrtc peer connection", peerconnectionRef.current);
      peerconnectionRef.current?.close();
      resizeObserver.disconnect();
      document.removeEventListener('fullscreenchange', handleFullscreenChange);
    };
  }, []);

  const showModal = () => {
    setIsModalOpen(true);
  };

  const handleFullScreen = () => {
    const videoWrapper = document.querySelector(`.${styles.videoWrapper}`) as HTMLElement;
    if (videoWrapper) {
      if (!isFullscreen) {
        if (videoWrapper.requestFullscreen) {
          videoWrapper.requestFullscreen();
        } else if ((videoWrapper as any).webkitRequestFullscreen) { /* Safari */
          (videoWrapper as any).webkitRequestFullscreen();
        } else if ((videoWrapper as any).msRequestFullscreen) { /* IE11 */
          (videoWrapper as any).msRequestFullscreen();
        }
      } else {
        if (document.exitFullscreen) {
          document.exitFullscreen();
        } else if ((document as any).webkitExitFullscreen) { /* Safari */
          (document as any).webkitExitFullscreen();
        } else if ((document as any).msExitFullscreen) { /* IE11 */
          (document as any).msExitFullscreen();
        }
      }
    }
  };
  
  const handleRequestControl = () => {

    const requestControlData = {
      accept: !acceptControl,
      accept_clipboard_sync: !acceptControl,
      accept_file_transfer: !acceptControl,
    } as API.SignalRequestControlData;
    sendSignalingMessage(SIGNALING_TYPE_CODE_REQUIRE_CONTROL, requestControlData);

  }

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
          var video_ref = remoteVideo.current!;
          video_ref.srcObject = event.streams[0];
          video_ref.autoplay = true;
          video_ref.controls = false;
          addRemoteVideoEvent(video_ref);
          break;
        case 'audio':
          var audio_ref = remoteAudio.current!;
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
        const offerModel = {
          offer: pc.localDescription!,
          desk_settings,
        } as OfferModel;

        console.log("event.candidate === null, offer_model: ", offerModel)

        sendSignalingMessage(SIGNALING_TYPE_CODE_OFFER, offerModel);
      }
    };


    // Offer to receive 1 audio, and 1 video track
    pc.addTransceiver('video', { 'direction': 'sendrecv' });
    pc.addTransceiver('audio', { 'direction': 'sendrecv' });

    //  create data channel for mouse and keyboard events before create offer
    let mouseEventDataChannel = pc.createDataChannel("mouse_event", { ordered: true });
    let keyboardEventDataChannel = pc.createDataChannel("keyboard_event", { ordered: true });

    mouseEventDataChannel.onopen = (event) => {
      console.log("mouse event data channel onopen, event=", event);
    };

    mouseEventDataChannel.onclose = (event) => {
      console.log("mouse event data channel onclose, event=", event);
    };

    mouseEventDataChannel.onerror = (event) => {
      console.log("mouse event data channel onerror, event=", event);
    };
    mouseEventDataChannel.onmessage = (event) => {
      console.log("mouse event data channel onmessage, event=", event);
    };

    keyboardEventDataChannel.onopen = (event) => {
      console.log("keyboard event data channel onopen, event=", event);
    };
    keyboardEventDataChannel.onclose = (event) => {
      console.log("keyboard event data channel onclose, event=", event);
    };
    keyboardEventDataChannel.onerror = (event) => {
      console.log("keyboard event data channel onerror, event=", event);
    };
    keyboardEventDataChannel.onmessage = (event) => {
      console.log("keyboard event data channel onmessage, event=", event);
    };

    mouseEventDataChannelRef.current = mouseEventDataChannel;
    keyboardEventDataChannelRef.current = keyboardEventDataChannel;

    pc.createOffer().then(d => {
      pc.setLocalDescription(d);
      const localDescriptionJson = JSON.stringify(d);
      console.info("create offer, local description=" + localDescriptionJson);
    }).catch((reason) => { console.log("Create offer failed, reason=", reason) });

    return pc;
  }

  const handleCancel = () => {
    setIsModalOpen(false);
  };

  const imageCaptureList = initSignalingData != null ? Object.keys(initSignalingData.video_device_list) : undefined;
  const imageCaptureSelectMap = imageCaptureList?.reduce((map, item) => {
    map.set(item, item);
    return map;
  }, new Map<string, string>);


  const audioCaptureList = initSignalingData != null ? Object.keys(initSignalingData.audio_device_list) : undefined;
  const audioCaptureSelectMap = audioCaptureList?.reduce((map, item) => {
    map.set(item, item);
    return map;
  }, new Map<string, string>);

  const videoEncodeTypeSelectMap = {
    "H264": "H.264",
    "VP8": "VP8",
    "VP9": "VP9"
  };

  const audioEncodeTypeSelectMap = {
    "OPUS": "Opus",
  };

  return (
    <PageContainer>
      <div
        className={styles.videoWrapper}
        onMouseEnter={() => setShowControls(true)}
        onMouseLeave={() => setShowControls(false)}
      >
        <video ref={remoteVideo} autoPlay muted className={styles.videoElement} tabIndex={0} />
        <div className={styles.controlBar}>
          <div className={styles.controlButtons}>
            <Tooltip title={acceptControl ? "退出控制" : "请求控制"}>
              <Button
                type="text"
                icon={acceptControl ? <StopOutlined /> : <CommentOutlined />}
                onClick={handleRequestControl}
                className={styles.controlButton}
              />
            </Tooltip>
            <Tooltip title={isFullscreen ? "退出全屏" : "全屏"}>
              <Button
                type="text"
                icon={isFullscreen ? <FullscreenExitOutlined /> : <FullscreenOutlined />}
                onClick={handleFullScreen}
                className={styles.controlButton}
              />
            </Tooltip>
            <Tooltip title="设置">
              <Button
                type="text"
                icon={<SettingOutlined />}
                onClick={showModal}
                className={styles.controlButton}
              />
            </Tooltip>
          </div>
        </div>
      </div>
      <audio ref={remoteAudio} autoPlay />
      <Divider />

      <FloatButton.Group
        /*open={true}*/
        shape="square"
        trigger="hover"
        /*style={{ insetInlineEnd: 24 }}*/
        icon={<CustomerServiceOutlined />}
      >
        <FloatButton tooltip={<div>全屏</div>} icon={<FullscreenOutlined />} onClick={handleFullScreen} />
        <FloatButton icon={<SettingOutlined />} onClick={showModal} />
        <FloatButton icon={<CommentOutlined />} tooltip={acceptControl ? <div>退出控制</div> : <div>请求控制</div>} onClick={handleRequestControl} />
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
                  const defaultAudioDevice = {
                    audio_data_flow: item.data_flow,
                    audio_device_id: null,
                  } as API.SelectedAudioDevice;
                  const defaultAudioDeviceJsonStr = JSON.stringify(defaultAudioDevice);
                  let found_value = map.get(defaultAudioDeviceJsonStr);
                  if (!found_value) {
                    map.set(defaultAudioDeviceJsonStr, `[${item.data_flow}]默认设备`);
                  }
                  const audioDevice = {
                    audio_data_flow: item.data_flow,
                    audio_device_id: item.id,
                  } as API.SelectedAudioDevice;
                  map.set(JSON.stringify(audioDevice), `[${item.data_flow}]${item.firendly_name}${item.default ? "(当前默认)" : ""}`);
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