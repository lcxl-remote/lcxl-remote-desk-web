use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use futures::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Child,
};

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

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    let mut stdout = child.stdout.take().expect("Failed to open stdout");
    let mut stderr = child.stderr.take().expect("Failed to open stderr");

    let stdout_buf = &mut [0; 1024];
    let stderr_buf = &mut [0; 1024];
    loop {
        tokio::select! {
            result = stdout.read(stdout_buf) => {
                if let Err(e) = result {
                    log::error!("Failed to read stdout: {}", e);
                    return Err(e.into());
                }
                let stdout_content = stdout_buf[..result.unwrap()].to_vec();
                log::debug!("Received stdout content and sending to client: {:?}", stdout_content);
                session.binary(stdout_content).await?;
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
                        stdin.write(text.as_bytes()).await?;
                    }

                    Ok(AggregatedMessage::Binary(bin)) => {
                        // echo binary message
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
