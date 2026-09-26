//! Portable native-shell discovery. Cross-process session routing stays closed.
use super::*;
impl HostControlHub {
    pub(crate) fn linux_ai_input_endpoint(&self) -> (u64, Option<String>) {
        if self.mode() != HubMode::Local {
            return (0, None);
        }
        self.inner.linux_ai_input_endpoint.lock().unwrap().clone()
    }
    pub(crate) fn publish_linux_ai_input_endpoint(&self, path: String) {
        if self.mode() != HubMode::Local {
            return;
        }
        let mut current = self.inner.linux_ai_input_endpoint.lock().unwrap();
        current.0 = current
            .0
            .checked_add(1)
            .expect("Input endpoint revision exhausted");
        current.1 = Some(path.clone());
        let _ = self.send_command(HostControlMessage::LinuxAiInputEndpoint {
            revision: current.0,
            path: Some(path),
        });
    }
    pub(crate) fn clear_linux_ai_input_endpoint(&self, path: &str) {
        let mut current = self.inner.linux_ai_input_endpoint.lock().unwrap();
        if current.1.as_deref() == Some(path) {
            current.0 = current
                .0
                .checked_add(1)
                .expect("Input endpoint revision exhausted");
            current.1 = None;
            let _ = self.send_command(HostControlMessage::LinuxAiInputEndpoint {
                revision: current.0,
                path: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_and_old_cleanup_preserve_current_endpoint_revision() {
        let hub = HostControlHub::new_local();
        assert_eq!(hub.linux_ai_input_endpoint(), (0, None));
        hub.publish_linux_ai_input_endpoint("first".into());
        hub.publish_linux_ai_input_endpoint("second".into());
        hub.clear_linux_ai_input_endpoint("first");
        assert_eq!(hub.linux_ai_input_endpoint(), (2, Some("second".into())));
        hub.clear_linux_ai_input_endpoint("second");
        assert_eq!(hub.linux_ai_input_endpoint(), (3, None));
    }
    #[test]
    fn service_aggregator_cannot_publish_an_unscoped_worker_endpoint() {
        let hub = HostControlHub::new_aggregator();
        hub.publish_linux_ai_input_endpoint("another-user".into());
        assert_eq!(hub.linux_ai_input_endpoint(), (0, None));
    }
}
