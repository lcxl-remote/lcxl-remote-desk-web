use std::process::ExitStatus;

use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytestring::ByteString;
use encoding_rs::{Decoder, Encoder};
use futures::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Child,
};
use windows::Win32::Globalization::GetOEMCP;

#[cfg(target_os = "windows")]
use crate::model::terminal::TerminalList;
use crate::{
    desk_error::DeskError,
    model::{settings::SharedSettings, user::CurrentUser},
};

#[cfg(target_os = "windows")]
pub fn fetch_terminal_list(settings: web::Data<SharedSettings>) -> Result<TerminalList, DeskError> {
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

    if let Ok(path) = which::which("wsl") {
        terminal_list.push(vec![path.to_string_lossy().into_owned()]);
    }

    return Ok(TerminalList {
        commands: terminal_list,
    });
}

pub fn convert_to_utf8_bytes(decoder: &mut Decoder, stdout_buf_vec: &mut Vec<u8>) -> Vec<u8> {
    let mut intermediate_buffer_bytes = [0u8; 4096];
    // Is there a safe way to create a stack-allocated &mut str?
    let intermediate_buffer: &mut str =
        unsafe { std::mem::transmute(&mut intermediate_buffer_bytes[..]) };
    let (_code_result, decoder_read, decoder_written, _) =
        decoder.decode_to_str(&stdout_buf_vec, intermediate_buffer, false);

    let removed: Vec<u8> = stdout_buf_vec.drain(0..decoder_read).collect();
    log::trace!("removed {} bytes from buffer", removed.len());
    let utf8_buffer = intermediate_buffer.as_bytes()[..decoder_written].to_vec();
    return utf8_buffer;
}

pub fn convert_str_to_encoding_bytes(encoder: &mut Encoder, utf8_byte_str: &ByteString) -> Vec<u8> {
    let mut utf8_str_buffer = utf8_byte_str.to_string();
    let mut output_vec = Vec::<u8>::new();
    loop {
        let mut intermediate_buffer_bytes = [0u8; 4096];
        let (_code_result, encoder_read, encoder_write, _) = encoder.encode_from_utf8(
            &utf8_str_buffer,
            intermediate_buffer_bytes.as_mut_slice(),
            false,
        );
        output_vec.extend_from_slice(&intermediate_buffer_bytes[..encoder_write]);
        let removed: Vec<char> = utf8_str_buffer.drain(0..encoder_read).collect();
        log::trace!("removed {} bytes from buffer", removed.len());
        if utf8_str_buffer.is_empty() {
            break;
        }
    }
    output_vec
}

pub fn check_process_exit_status(child: &mut Child) -> Option<ExitStatus> {
    let wait_result = child.try_wait();
    if let Ok(Some(status)) = wait_result {
        Some(status)
    } else {
        None
    }
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

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    let mut stdout = child.stdout.take().expect("Failed to open stdout");
    let mut stderr = child.stderr.take().expect("Failed to open stderr");

    let stdout_buf = &mut [0; 1024];
    let stderr_buf = &mut [0; 1024];

    let mut stdout_buf_vec = Vec::<u8>::with_capacity(1024);
    let mut stderr_buf_vec = Vec::<u8>::with_capacity(1024);

    loop {
        tokio::select! {
            result = stdout.read(stdout_buf) => {
                if let Err(e) = result {
                    log::error!("Failed to read stdout: {}", e);
                    return Err(e.into());
                }
                if let Some(status) = check_process_exit_status(&mut child) {
                    log::warn!("Process exited with status: {:?}", status);
                    break;
                }
                stdout_buf_vec.extend_from_slice(&stdout_buf[..result.unwrap()]); // Extend the vector

                log::debug!("Received stdout content and sending to client: {:?}", stdout_buf_vec);

                let utf8_buffer = convert_to_utf8_bytes(&mut decoder, &mut stdout_buf_vec);
                session.binary(utf8_buffer).await?;
            },
            result = stderr.read(stderr_buf) => {
                if let Err(e) = result {
                    log::error!("Failed to read stderr: {}", e);
                    return Err(e.into());
                }
                if let Some(status) = check_process_exit_status(&mut child) {
                    log::warn!("Process exited with status: {:?}", status);
                    break;
                }
                stderr_buf_vec.extend_from_slice(&stderr_buf[..result.unwrap()]); // Extend the vector

                log::debug!("Received stderr content and sending to client: {:?}", stderr_buf_vec);

                let utf8_buffer = convert_to_utf8_bytes(&mut decoder, &mut stderr_buf_vec);
                session.binary(utf8_buffer).await?;
            },
            result = stream.next() => {
                if result.is_none() {
                    log::info!("Stream closed");
                    break;
                }
                if let Some(status) = check_process_exit_status(&mut child) {
                    log::warn!("Process exited with status: {:?}", status);
                    break;
                }
                let msg = result.unwrap();
                match msg {
                    Ok(AggregatedMessage::Text(text)) => {
                        //stdin_buf_vec
                        log::debug!("Recevied text content from websocket and sending to stdin: {:?}", text);
                        let encoding_buffer = convert_str_to_encoding_bytes(&mut encoder, &text);

                        log::debug!("Write encoding buffer to stdin: {:?}", encoding_buffer);
                        stdin.write_all(&encoding_buffer).await?;
                        stdin.flush().await?;
                    }

                    Ok(AggregatedMessage::Binary(bin)) => {
                        log::debug!("Recevied binary content from websocket and sending to stdin: {:?}", bin);
                        stdin.write_all(&bin).await?;
                        stdin.flush().await?;
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
        assert!(!result.commands.is_empty()); // Ensure that the terminal list is not empty
        Ok(())
    }
}
