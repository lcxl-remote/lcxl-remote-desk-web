//! Bounded native failures, independent of any operation's lifecycle.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStage {
    Authorization,
    TargetResolution,
    Environment,
    HelperStart,
    HelperCommunication,
    ProcessCreation,
    ProcessContainment,
    OutputRead,
    ProcessWait,
    ProcessCleanup,
    ServiceEnumeration,
    ServiceConfiguration,
    Query,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct NativeDiagnostic {
    pub stage: DiagnosticStage,
    pub operation: String,
    pub domain: String,
    pub code: Option<i64>,
    pub name: Option<String>,
    pub message: String,
}

impl NativeDiagnostic {
    pub fn new(
        stage: DiagnosticStage,
        operation: &str,
        domain: &str,
        code: Option<i64>,
        message: &str,
    ) -> Self {
        Self {
            stage,
            operation: bounded(operation, 128),
            domain: bounded(domain, 128),
            code,
            name: None,
            message: bounded(
                if message.is_empty() {
                    "No readable system message is available; inspect the native domain and code"
                } else {
                    message
                },
                1024,
            ),
        }
    }

    pub fn is_bounded(&self) -> bool {
        !self.operation.is_empty()
            && self.operation.len() <= 128
            && !self.domain.is_empty()
            && self.domain.len() <= 128
            && self.message.len() <= 1024
            && self.name.as_ref().is_none_or(|name| name.len() <= 256)
    }

    pub fn from_io(stage: DiagnosticStage, operation: &str, error: &std::io::Error) -> Self {
        let domain = if error.raw_os_error().is_none() {
            "io"
        } else if cfg!(windows) {
            "win32"
        } else {
            "posix"
        };
        Self::new(
            stage,
            operation,
            domain,
            error.raw_os_error().map(i64::from),
            &error.to_string(),
        )
    }
}

fn bounded(text: &str, bytes: usize) -> String {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_preserves_code_and_bounds_utf8() {
        let d = NativeDiagnostic::new(
            DiagnosticStage::ProcessCreation,
            "spawn",
            "win32",
            Some(740),
            &"错误".repeat(1024),
        );
        assert!(d.message.len() <= 1024);
        assert_eq!(d.code, Some(740));
        assert!(d.is_bounded());
        let bytes = wincode::serialize(&d).unwrap();
        assert_eq!(wincode::deserialize::<NativeDiagnostic>(&bytes).unwrap(), d);
        assert_eq!(
            serde_json::from_str::<NativeDiagnostic>(&serde_json::to_string(&d).unwrap()).unwrap(),
            d
        );
    }
}
