import sys

def replace_lib():
    with open('server/src/lib.rs', 'r', encoding='utf-8') as f:
        content = f.read()

    target = """    let local_node_token = uuid::Uuid::new_v4().to_string();
    let validator: Arc<dyn desk_signal_facade::service::NodeTokenValidator> =
        Arc::new(crate::service::signaling::LocalNodeTokenValidator {
            settings: shared_settings_data.clone(),
            local_node_token: local_node_token.clone(),
        });
    let validator_data = web::Data::new(validator);

    // start desk session if mode is Default or DeskServer
    if startup_mode == StartupMode::Default || startup_mode == StartupMode::DeskServer {
        info!("Starting desk session");
        let settings_clone = shared_settings_data.clone();
        actix_web::rt::spawn(async move {
            if let Err(e) = start_desk_session(settings_clone, channels, local_node_token).await {
                error!("Desk session error: {}", e);
            }
        });
    }"""
    
    replacement = """    // If this instance runs signaling, ensure local_signaling_token is generated and persisted
    if startup_mode == StartupMode::Default || startup_mode == StartupMode::Signaling {
        let mut s = shared_settings_data.write().await;
        if s.system.local_signaling_token.is_none() {
            let token = uuid::Uuid::new_v4().to_string();
            info!("Generated new local_signaling_token: {}", token);
            s.system.local_signaling_token = Some(token);
            if let Err(e) = s.save() {
                error!("Failed to save local_signaling_token: {}", e);
            }
        }
    }

    let validator: Arc<dyn desk_signal_facade::service::NodeTokenValidator> =
        Arc::new(crate::service::signaling::LocalNodeTokenValidator {
            settings: shared_settings_data.clone(),
        });
    let validator_data = web::Data::new(validator);

    // start desk session if mode is Default or DeskServer
    if startup_mode == StartupMode::Default || startup_mode == StartupMode::DeskServer {
        info!("Starting desk session");
        let settings_clone = shared_settings_data.clone();
        let startup_mode_clone = startup_mode.clone();
        actix_web::rt::spawn(async move {
            if let Err(e) = start_desk_session(settings_clone, channels, startup_mode_clone).await {
                error!("Desk session error: {}", e);
            }
        });
    }"""

    if target in content:
        content = content.replace(target, replacement)
        with open('server/src/lib.rs', 'w', encoding='utf-8') as f:
            f.write(content)
        print("lib.rs Replaced successfully")
    else:
        print("Target string not found in lib.rs")

if __name__ == "__main__":
    replace_lib()
