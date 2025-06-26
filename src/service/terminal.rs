use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use encoding_rs::Encoding;
use futures::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Child,
};
use windows::Win32::Globalization::GetOEMCP;

use crate::{
    desk_error::DeskError,
    model::{settings::SharedSettings, user::CurrentUser},
};

#[cfg(target_os = "windows")]
pub fn fetch_terminal_list(
    settings: web::Data<SharedSettings>,
) -> Result<Vec<Vec<String>>, DeskError> {
    let mut terminal_list = Vec::<Vec<String>>::new();

    if let Ok(path) = which::which("cmd") {
        terminal_list.push(vec![path.to_string_lossy().into_owned()]);
    }

    if let Ok(path) = which::which("powershell") {
        terminal_list.push(vec![path.to_string_lossy().into_owned()]);
    }

    if let Ok(path) = which::which("bash") {
        terminal_list.push(vec![path.to_string_lossy().into_owned()]);
    }

    return Ok(terminal_list);
}

pub async fn handle_terminal(
    settings: web::Data<SharedSettings>,
    mut stream: AggregatedMessageStream,
    mut session: Session,
    user: CurrentUser,
    mut child: Child,
) -> Result<(), DeskError> {
    log::info!("Handling terminal session");
    // get oem code page
    let oemcp = unsafe { GetOEMCP() };
    log::info!("OEM Code Page: {}", oemcp);
    let encoding = codepage::to_encoding(oemcp as u16).unwrap();

    let mut decoder = encoding.new_decoder();
    let mut encoder = encoding.new_encoder();

    let mut intermediate_buffer_bytes = [0u8; 4096];
    // Is there a safe way to create a stack-allocated &mut str?
    let intermediate_buffer: &mut str =
        unsafe { std::mem::transmute(&mut intermediate_buffer_bytes[..]) };

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    let mut stdout = child.stdout.take().expect("Failed to open stdout");
    let mut stderr = child.stderr.take().expect("Failed to open stderr");

    let stdout_buf = &mut [0; 1024];
    let stderr_buf = &mut [0; 1024];

    let mut stdout_buf_vec = Vec::<u8>::with_capacity(1024);

    loop {
        tokio::select! {
            result = stdout.read(stdout_buf) => {
                if let Err(e) = result {
                    log::error!("Failed to read stdout: {}", e);
                    return Err(e.into());
                }
                stdout_buf_vec.extend_from_slice(&stdout_buf[..result.unwrap()]); // Extend the vector
                //let stdout_content = stdout_buf[..result.unwrap()].to_vec();
                log::debug!("Received stdout content and sending to client: {:?}", stdout_buf_vec);

                let (code_result, decoder_read, decoder_written, _) = decoder.decode_to_str(&stdout_buf_vec,  intermediate_buffer, false);

                let removed: Vec<u8> = stdout_buf_vec.drain(0..decoder_read).collect();
                let utf8_buffer = intermediate_buffer.as_bytes()[..decoder_written].to_vec();

                session.binary(utf8_buffer).await?;
            },
            result = stderr.read(stderr_buf) => {
                if let Err(e) = result {
                    log::error!("Failed to read stdout: {}", e);
                    return Err(e.into());
                }
                let stderr_content = stderr_buf[..result.unwrap()].to_vec();
                log::debug!("Received stderr content and sending to client: {:?}", stderr_content);
                session.binary(stderr_content).await?;
            },
            result = stream.next() => {
                if result.is_none() {
                    log::info!("Stream closed");
                    break;
                }
                let msg = result.unwrap();
                match msg {
                    Ok(AggregatedMessage::Text(text)) => {
                        log::debug!("Recevied text content from websocket and sending to stdin: {:?}", text);
                        stdin.write(text.as_bytes()).await?;
                    }

                    Ok(AggregatedMessage::Binary(bin)) => {
                       log::debug!("Recevied binary content from websocket and sending to stdin: {:?}", text);
                    stdin.write(&bin).await?;
                    }

                    Ok(AggregatedMessage::Ping(msg)) => {
                        // respond to PING frame with PONG frame
                        session.pong(&msg).await?;
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
                }
            },
        };
    }

    Ok(())
}

mod tests {
    use crate::model::settings::Settings;

    use super::*;

    #[test]
    fn test_fetch_terminal_list() -> Result<(), DeskError> {
        let settings = web::Data::new(SharedSettings::from(Settings::default()));
        let result = fetch_terminal_list(settings)?;
        println!("Terminal list: {:?}", result);
        assert!(!result.is_empty()); // Ensure that the terminal list is not empty
        Ok(())
    }
}
