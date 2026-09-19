use super::*;

/// No containment primitive is wired for this platform, so execution is
/// refused rather than run unreclaimably.
pub struct Platform;

impl Platform {
    pub fn prepare(_generation: &str) -> Result<(Self, Option<String>), ContainmentError> {
        Err(ContainmentError::message(
            "this platform has no process-tree containment, so execution is refused",
        ))
    }
    pub fn apply(&self, _cmd: &mut Command) {}
    pub fn adopt(&mut self, _child: &Child) -> Result<(), ContainmentError> {
        Err(ContainmentError::message("unsupported platform"))
    }
    pub fn identity_after_adopt(&self) -> Option<String> {
        None
    }
    pub fn reclaim(
        &mut self,
    ) -> Result<(), desk_agent_protocol::native_diagnostic::NativeDiagnostic> {
        Ok(())
    }
}
