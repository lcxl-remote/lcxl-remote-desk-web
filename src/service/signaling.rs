use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use futures_util::StreamExt;

pub async fn handle_signaling(mut stream: AggregatedMessageStream, mut session: Session) {
    // Handle signaling logic here
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(AggregatedMessage::Text(text)) => {
                // echo text message
                session.text(text).await.unwrap();
            }

            Ok(AggregatedMessage::Binary(bin)) => {
                // echo binary message
                session.binary(bin).await.unwrap();
            }

            Ok(AggregatedMessage::Ping(msg)) => {
                // respond to PING frame with PONG frame
                session.pong(&msg).await.unwrap();
            }
            Ok(AggregatedMessage::Pong(_)) => {
                // ignore PONG frames
            }
            Ok(AggregatedMessage::Close(close_reason)) => {
                log::warn!("WS close frame received: {:?}", close_reason);
                break;
            }
            Err(e) => {
                log::error!("WS error: {}", e);
                break;
            }
            _ => {}
        }
    }
}
