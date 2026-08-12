pub mod host_control_factory;
#[cfg(target_os = "linux")]
pub mod input_grab;
#[cfg(target_os = "linux")]
pub mod linux_host_control;

#[cfg(target_os = "windows")]
pub mod windows_host_control;

#[cfg(target_os = "macos")]
pub mod mac_host_control;

use crate::model::host_control::PrivateScreenCommand;

fn send_private_screen_command(
    sender: Option<&std::sync::mpsc::Sender<PrivateScreenCommand>>,
    from_connection_id: &str,
    request_id: &str,
    enable: bool,
) {
    let Some(sender) = sender else {
        log::warn!(
            "Private screen command sender is not configured (maybe starting as standalone server)"
        );
        return;
    };

    let command = if enable {
        PrivateScreenCommand::Show {
            connection_id: from_connection_id.to_string(),
            request_id: request_id.to_string(),
        }
    } else {
        PrivateScreenCommand::Hide {
            connection_id: from_connection_id.to_string(),
            request_id: request_id.to_string(),
        }
    };
    if let Err(error) = sender.send(command) {
        log::error!("Failed to send private screen command: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_screen_command_maps_enable_to_show_and_disable_to_hide() {
        let (sender, receiver) = std::sync::mpsc::channel();

        send_private_screen_command(Some(&sender), "connection", "request-1", true);
        assert!(matches!(
            receiver.recv().unwrap(),
            PrivateScreenCommand::Show { connection_id, request_id }
                if connection_id == "connection" && request_id == "request-1"
        ));

        send_private_screen_command(Some(&sender), "connection", "request-2", false);
        assert!(matches!(
            receiver.recv().unwrap(),
            PrivateScreenCommand::Hide { connection_id, request_id }
                if connection_id == "connection" && request_id == "request-2"
        ));
    }

    #[test]
    fn missing_private_screen_sender_is_a_noop() {
        send_private_screen_command(None, "connection", "request", true);
    }
}
