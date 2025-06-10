import { querySettings } from "@/services/desk/querySettings";
import { updateSettings } from "@/services/desk/updateSettings";
import { PageContainer, ProForm, ProFormDigit, ProFormSelect, ProFormSwitch, ProFormText } from "@ant-design/pro-components";
import { useIntl, useModel } from "@umijs/max";
import { Alert, Divider, message } from "antd";
import { useEffect, useRef, useState } from "react";

import styles from './index.less'; // 告诉 umi 编译这个 less

const SIGNALING_TYPE_CODE_INIT = 0;
const SIGNALING_TYPE_CODE_OFFER = 100;
const SIGNALING_TYPE_CODE_ANSWER = 101;
const SIGNALING_TYPE_CODE_CANID = 200;
const SIGNALING_TYPE_CODE_ERROR = 1000;

type SignalingModel = {
  /**
   * Signaling type
   */
  signaling_type: number;
  /** 
   * Check if signaling is succeed
   */
  signaling_success: boolean;
  /** 
   * Signaling status code
   */
  signaling_status_code: number;
  /** 
   * Signaling message
   * 
   */
  signaling_message?: string;

  /**
   * Signaling data
   */
  signaling_data?: string;
};

type InitSignalingData = {
  ice_servers: RTCIceServer[],
  user_name: string,
};


const Desk: React.FC = () => {
  const { initialState, setInitialState } = useModel('@@initialState');
  const intl = useIntl();
  const remote_video = useRef<HTMLVideoElement>(null);
  const remote_audio = useRef<HTMLAudioElement>(null);
  const socketRef = useRef<WebSocket>();
  const peerconnectionRef = useRef<RTCPeerConnection>();
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
        const signaling_model = JSON.parse(event.data) as SignalingModel;
        if (!signaling_model.signaling_success) {
          console.error("Received error data", signaling_model);
          return;
        }
        switch (signaling_model.signaling_type) {
          case SIGNALING_TYPE_CODE_INIT:
            const init_signaling_data = JSON.parse(signaling_model.signaling_data!) as InitSignalingData;

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
                  signaling_success: true,
                  signaling_data: local_description_json,
                  signaling_status_code: 0,
                } as SignalingModel;
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
      return () => {
        console.log("关闭websocket", sock);
        sock.close();
        console.log("关闭webrtc peer connection", peerconnectionRef.current);
        peerconnectionRef.current?.close();
      };
    })();
  }, []);


  return (
    <PageContainer>
      <video ref={remote_video} autoPlay muted className={styles.videoContainer} />
      <audio ref={remote_audio} autoPlay />
      <Divider />

    </PageContainer>

  );
}

export default Desk;