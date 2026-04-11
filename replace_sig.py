import sys

def replace_sig():
    with open('server/src/service/signaling.rs', 'r', encoding='utf-8') as f:
        content = f.read()

    target_1 = """                    desk_signal_facade::model::system_settings::RemoteSystemSettings {
                        enable_ipv6: settings.system.enable_ipv6,
                        port: settings.system.port,
                        listen_addr_ipv4: settings.system.listen_addr_ipv4.clone(),
                        listen_addr_ipv6: settings.system.listen_addr_ipv6.clone(),
                        locale: settings.system.locale.clone(),
                        signaling_url: settings.system.signaling_url.clone(),
                        auto_start: settings.system.auto_start,
                        manager_api_token: settings.system.manager_api_token.clone(),
                    }"""
    
    replace_1 = """                    desk_signal_facade::model::system_settings::RemoteSystemSettings {
                        enable_ipv6: settings.system.enable_ipv6,
                        port: settings.system.port,
                        listen_addr_ipv4: settings.system.listen_addr_ipv4.clone(),
                        listen_addr_ipv6: settings.system.listen_addr_ipv6.clone(),
                        locale: settings.system.locale.clone(),
                        signaling_url: settings.system.signaling_url.clone(),
                        signaling_token: settings.system.signaling_token.clone(),
                        manager_url: settings.system.manager_url.clone(),
                        auto_start: settings.system.auto_start,
                        manager_api_token: settings.system.manager_api_token.clone(),
                    }"""

    target_2 = """                {
                    let mut settings = self.settings.write().await;
                    settings.system.enable_ipv6 = remote_settings.enable_ipv6;
                    settings.system.port = remote_settings.port;
                    settings.system.listen_addr_ipv4 = remote_settings.listen_addr_ipv4;
                    settings.system.listen_addr_ipv6 = remote_settings.listen_addr_ipv6;
                    settings.system.locale = remote_settings.locale;
                    settings.system.signaling_url = remote_settings.signaling_url;
                    settings.system.auto_start = remote_settings.auto_start;
                    settings.system.manager_api_token = remote_settings.manager_api_token;
                    settings.save()?;
                }"""

    replace_2 = """                {
                    let mut settings = self.settings.write().await;
                    settings.system.enable_ipv6 = remote_settings.enable_ipv6;
                    settings.system.port = remote_settings.port;
                    settings.system.listen_addr_ipv4 = remote_settings.listen_addr_ipv4;
                    settings.system.listen_addr_ipv6 = remote_settings.listen_addr_ipv6;
                    settings.system.locale = remote_settings.locale;
                    settings.system.signaling_url = remote_settings.signaling_url;
                    settings.system.signaling_token = remote_settings.signaling_token;
                    settings.system.manager_url = remote_settings.manager_url;
                    settings.system.auto_start = remote_settings.auto_start;
                    settings.system.manager_api_token = remote_settings.manager_api_token;
                    settings.save()?;
                }"""

    if target_1 in content:
        content = content.replace(target_1, replace_1)
        print("Replaced chunk 1")
    else:
        print("Chunk 1 not found")

    if target_2 in content:
        content = content.replace(target_2, replace_2)
        print("Replaced chunk 2")
    else:
        print("Chunk 2 not found")

    with open('server/src/service/signaling.rs', 'w', encoding='utf-8') as f:
        f.write(content)

if __name__ == "__main__":
    replace_sig()
