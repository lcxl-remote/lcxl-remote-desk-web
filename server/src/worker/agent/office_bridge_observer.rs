//! Read-only client for the paired local Office.js bridge.
//!
//! The bridge is deliberately a loopback HTTPS endpoint. Its task pane owns the
//! Office.js runtime; the worker never automates Excel through COM/UIA and never
//! receives a mutation credential on this path. The admin bearer is read from a
//! local file supplied by the trusted host process and is never logged.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use desk_agent_protocol::computer_use::{ExcelCellProjection, OfficeCellValue};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const BASE_URL_ENV: &str = "LRD_OFFICE_BRIDGE_BASE_URL";
const TOKEN_FILE_ENV: &str = "LRD_OFFICE_BRIDGE_ADMIN_TOKEN_FILE";
const CA_CERT_FILE_ENV: &str = "LRD_OFFICE_BRIDGE_CA_CERT_FILE";
const MAX_CA_CERT_BYTES: usize = 64 * 1024;
const MAX_BRIDGE_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_EXCEL_CELLS: usize = 16;
const MAX_FORMULA_BYTES: usize = 512;
const MAX_CELL_TEXT_BYTES: usize = 4 * 1024;
const MAX_NUMBER_FORMAT_BYTES: usize = 128;
const POLL_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Clone, Debug)]
struct BridgeConfig {
    base_url: String,
    admin_token: String,
    ca_cert_pem: Vec<u8>,
}

impl BridgeConfig {
    fn load() -> Result<Self, AgentError> {
        let base_url = std::env::var(BASE_URL_ENV)
            .map_err(|_| unavailable("the local Office.js bridge is not configured"))?;
        let parsed = url::Url::parse(base_url.trim())
            .map_err(|_| invalid("the Office bridge URL is invalid"))?;
        let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
        if parsed.scheme() != "https"
            || !matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !matches!(parsed.path(), "" | "/")
        {
            return Err(invalid(
                "the Office bridge must be a credential-free loopback HTTPS origin",
            ));
        }
        let token_path =
            PathBuf::from(std::env::var(TOKEN_FILE_ENV).map_err(|_| {
                unavailable("the Office bridge admin token file is not configured")
            })?);
        if !token_path.is_absolute() {
            return Err(invalid(
                "the Office bridge admin token path must be absolute",
            ));
        }
        let admin_token = std::fs::read_to_string(&token_path)
            .map_err(|_| unavailable("the Office bridge admin token cannot be read"))?;
        let admin_token = admin_token.trim().to_string();
        if admin_token.len() < 24 || admin_token.len() > 512 {
            return Err(invalid("the Office bridge admin token is malformed"));
        }
        let ca_cert_path = PathBuf::from(
            std::env::var(CA_CERT_FILE_ENV)
                .map_err(|_| unavailable("the Office bridge CA certificate is not configured"))?,
        );
        if !ca_cert_path.is_absolute() {
            return Err(invalid(
                "the Office bridge CA certificate path must be absolute",
            ));
        }
        let ca_cert_pem = std::fs::read(&ca_cert_path)
            .map_err(|_| unavailable("the Office bridge CA certificate cannot be read"))?;
        if ca_cert_pem.is_empty() || ca_cert_pem.len() > MAX_CA_CERT_BYTES {
            return Err(invalid("the Office bridge CA certificate is malformed"));
        }
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            admin_token,
            ca_cert_pem,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ExcelSelectionObservation {
    pub document_url_hash: String,
    pub address: String,
    pub row_count: u32,
    pub column_count: u32,
    pub cells: Vec<ExcelCellProjection>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionList {
    sessions: Vec<BridgeSession>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeSession {
    session_id: String,
    host: String,
    document_url_hash: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QueueAction<'a> {
    session_id: &'a str,
    action: InspectAction<'a>,
}

#[derive(Debug, Serialize)]
struct InspectAction<'a> {
    id: &'a str,
    kind: &'static str,
    generation: u64,
}

#[derive(Debug, Deserialize)]
struct Completion {
    result: CompletionResult,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status")]
enum CompletionResult {
    Verified { value: RawExcelSelection },
    Failed { error: BridgeFailure },
}

#[derive(Debug, Deserialize)]
struct BridgeFailure {
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawExcelSelection {
    address: String,
    row_count: u32,
    column_count: u32,
    formulas: Vec<Vec<Value>>,
    values: Vec<Vec<Value>>,
    number_format: Vec<Vec<Value>>,
}

pub(crate) fn configured() -> bool {
    BridgeConfig::load().is_ok()
}

pub(crate) fn current_excel_document_hash() -> Option<String> {
    let Ok(config) = BridgeConfig::load() else {
        return None;
    };
    let Ok(agent) = bridge_agent(&config) else {
        return None;
    };
    let sessions = get_json::<SessionList>(
        &agent,
        &format!("{}/admin/sessions", config.base_url),
        &config.admin_token,
        64 * 1024,
    )
    .ok()?;
    let mut candidates = sessions
        .sessions
        .into_iter()
        .filter(|session| session.host == "Excel")
        .filter_map(|session| session.document_url_hash);
    let document_hash = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some(document_hash)
}

pub(crate) fn inspect_excel_selection(
    expected_document_hash: Option<&str>,
    max_objects: u32,
    max_bytes: u32,
) -> Result<ExcelSelectionObservation, AgentError> {
    if max_objects == 0 || max_objects as usize > MAX_EXCEL_CELLS {
        return Err(invalid("Excel selection max_objects must be within 1..=16"));
    }
    if max_bytes == 0 || max_bytes as usize > MAX_BRIDGE_RESPONSE_BYTES {
        return Err(invalid(
            "Excel selection max_bytes must be within 1..=262144",
        ));
    }
    let config = BridgeConfig::load()?;
    let agent = bridge_agent(&config)?;
    let sessions: SessionList = get_json(
        &agent,
        &format!("{}/admin/sessions", config.base_url),
        &config.admin_token,
        64 * 1024,
    )?;
    let mut candidates = sessions.sessions.into_iter().filter(|session| {
        session.host == "Excel"
            && session
                .document_url_hash
                .as_deref()
                .is_some_and(|hash| expected_document_hash.is_none_or(|expected| hash == expected))
    });
    let session = candidates
        .next()
        .ok_or_else(|| unavailable("no paired Excel document is available"))?;
    if candidates.next().is_some() {
        return Err(invalid(
            "more than one paired Excel document is available; select a document explicitly",
        ));
    }
    let document_url_hash = session
        .document_url_hash
        .ok_or_else(|| unavailable("the paired Excel document has no stable document identity"))?;
    if document_url_hash.len() != 64
        || !document_url_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid("the paired Excel document identity is malformed"));
    }

    let action_id = format!("inspect-{}", uuid::Uuid::new_v4());
    let request = QueueAction {
        session_id: &session.session_id,
        action: InspectAction {
            id: &action_id,
            kind: "inspect_selection",
            generation: 1,
        },
    };
    post_json::<_, Value>(
        &agent,
        &format!("{}/admin/actions", config.base_url),
        &config.admin_token,
        &request,
        32 * 1024,
    )?;

    let deadline = Instant::now() + POLL_TIMEOUT;
    let raw = loop {
        let value: Value = get_json(
            &agent,
            &format!("{}/admin/actions/{action_id}", config.base_url),
            &config.admin_token,
            max_bytes as usize,
        )?;
        if value.get("result").is_some() {
            let completion: Completion = serde_json::from_value(value)
                .map_err(|_| transport("the Office bridge returned an invalid completion"))?;
            match completion.result {
                CompletionResult::Verified { value } => break value,
                CompletionResult::Failed { error } => {
                    return Err(match error.code.as_str() {
                        "selection_too_large" => {
                            invalid("the Excel selection is too large; select at most 16 cells")
                        }
                        "office_inspection_failed" => {
                            transport("the Excel task pane could not inspect the current selection")
                        }
                        _ => transport("the Office bridge returned an unknown failure code"),
                    });
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(timeout("timed out waiting for the Excel task pane"));
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    validate_shape(&raw, max_objects as usize)?;
    let cells = project_cells(&raw)?;
    Ok(ExcelSelectionObservation {
        document_url_hash,
        address: bounded_text(raw.address, 512, "Excel selection address")?,
        row_count: raw.row_count,
        column_count: raw.column_count,
        cells,
    })
}

fn bridge_agent(config: &BridgeConfig) -> Result<ureq::Agent, AgentError> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};

    let certificate = CertificateDer::from_pem_slice(&config.ca_cert_pem)
        .map_err(|_| invalid("the Office bridge CA certificate is not valid PEM"))?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(certificate)
        .map_err(|_| invalid("the Office bridge CA certificate cannot be trusted"))?;
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(ureq::AgentBuilder::new()
        .tls_config(Arc::new(tls))
        .timeout_connect(Duration::from_secs(3))
        .timeout_read(Duration::from_secs(5))
        .timeout_write(Duration::from_secs(5))
        .redirects(0)
        .build())
}

fn validate_shape(raw: &RawExcelSelection, max_objects: usize) -> Result<(), AgentError> {
    let rows = raw.row_count as usize;
    let columns = raw.column_count as usize;
    let cells = rows
        .checked_mul(columns)
        .ok_or_else(|| invalid("Excel selection dimensions overflow"))?;
    if rows == 0 || columns == 0 || cells > max_objects || cells > MAX_EXCEL_CELLS {
        return Err(invalid(
            "Excel selection exceeds the requested object bound",
        ));
    }
    for matrix in [&raw.formulas, &raw.values, &raw.number_format] {
        if matrix.len() != rows || matrix.iter().any(|row| row.len() != columns) {
            return Err(transport(
                "the Office bridge returned a ragged Excel matrix",
            ));
        }
    }
    Ok(())
}

fn project_cells(raw: &RawExcelSelection) -> Result<Vec<ExcelCellProjection>, AgentError> {
    let mut cells = Vec::with_capacity((raw.row_count * raw.column_count) as usize);
    for row in 0..raw.row_count as usize {
        for column in 0..raw.column_count as usize {
            let formula = match &raw.formulas[row][column] {
                Value::String(value) if value.starts_with('=') => Some(bounded_text(
                    value.clone(),
                    MAX_FORMULA_BYTES,
                    "Excel formula",
                )?),
                _ => None,
            };
            let value = project_value(&raw.values[row][column])?;
            let number_format = match &raw.number_format[row][column] {
                Value::String(value) if !value.is_empty() => Some(bounded_text(
                    value.clone(),
                    MAX_NUMBER_FORMAT_BYTES,
                    "Excel number format",
                )?),
                Value::Null | Value::String(_) => None,
                _ => {
                    return Err(transport(
                        "the Office bridge returned an invalid number format",
                    ));
                }
            };
            cells.push(ExcelCellProjection {
                row_offset: row as u32,
                column_offset: column as u32,
                formula,
                value,
                number_format,
            });
        }
    }
    Ok(cells)
}

fn project_value(value: &Value) -> Result<OfficeCellValue, AgentError> {
    match value {
        Value::Null => Ok(OfficeCellValue::Blank),
        Value::Bool(value) => Ok(OfficeCellValue::Boolean(*value)),
        Value::Number(value) => Ok(OfficeCellValue::Number(value.to_string())),
        Value::String(value) if value.is_empty() => Ok(OfficeCellValue::Blank),
        Value::String(value) if value.starts_with('#') => Ok(OfficeCellValue::Error(bounded_text(
            value.clone(),
            MAX_CELL_TEXT_BYTES,
            "Excel error",
        )?)),
        Value::String(value) => Ok(OfficeCellValue::Text(bounded_text(
            value.clone(),
            MAX_CELL_TEXT_BYTES,
            "Excel text value",
        )?)),
        Value::Array(_) | Value::Object(_) => Err(transport(
            "the Office bridge returned a non-scalar Excel cell value",
        )),
    }
}

fn bounded_text(value: String, max_bytes: usize, label: &str) -> Result<String, AgentError> {
    if value.len() > max_bytes {
        Err(error(
            AgentErrorKind::OutputLimitExceeded,
            format!("{label} exceeds its byte bound"),
            false,
        ))
    } else {
        Ok(value)
    }
}

fn get_json<T: for<'de> Deserialize<'de>>(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    max_bytes: usize,
) -> Result<T, AgentError> {
    let response = agent
        .get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json")
        .call()
        .map_err(map_http_error)?;
    read_json(response, max_bytes)
}

fn post_json<B: Serialize, T: for<'de> Deserialize<'de>>(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    body: &B,
    max_bytes: usize,
) -> Result<T, AgentError> {
    let response = agent
        .post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json")
        .send_json(body)
        .map_err(map_http_error)?;
    read_json(response, max_bytes)
}

fn read_json<T: for<'de> Deserialize<'de>>(
    response: ureq::Response,
    max_bytes: usize,
) -> Result<T, AgentError> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| transport("failed to read the Office bridge response"))?;
    if bytes.len() > max_bytes {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            "the Office bridge response exceeds its byte bound",
            false,
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| transport("the Office bridge returned invalid JSON"))
}

fn map_http_error(source: ureq::Error) -> AgentError {
    match source {
        ureq::Error::Status(code, _) if code == 401 || code == 403 => error(
            AgentErrorKind::PermissionDenied,
            "the Office bridge rejected its local credential",
            false,
        ),
        ureq::Error::Status(_, _) => transport("the Office bridge rejected the request"),
        ureq::Error::Transport(_) => transport("the local Office bridge is unreachable"),
    }
}

fn unavailable(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::SessionUnavailable, message, true)
}

fn invalid(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::InvalidInput, message, false)
}

fn timeout(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::Timeout, message, true)
}

fn transport(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::TransportError, message, true)
}

fn error(kind: AgentErrorKind, message: impl Into<String>, retryable: bool) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_nested_cell_values_and_projects_formulas() {
        assert!(project_value(&serde_json::json!({"secret": true})).is_err());
        let raw = RawExcelSelection {
            address: "Sheet1!A1:B1".into(),
            row_count: 1,
            column_count: 2,
            formulas: vec![vec![serde_json::json!("=1+1"), serde_json::json!("plain")]],
            values: vec![vec![serde_json::json!(2), serde_json::json!("ok")]],
            number_format: vec![vec![
                serde_json::json!("General"),
                serde_json::json!("General"),
            ]],
        };
        validate_shape(&raw, 2).unwrap();
        let cells = project_cells(&raw).unwrap();
        assert_eq!(cells[0].formula.as_deref(), Some("=1+1"));
        assert_eq!(cells[1].formula, None);
        assert_eq!(cells[0].value, OfficeCellValue::Number("2".into()));
    }

    #[test]
    fn selection_shape_is_strictly_bounded() {
        let raw = RawExcelSelection {
            address: "Sheet1!A1:B1".into(),
            row_count: 1,
            column_count: 2,
            formulas: vec![vec![Value::Null, Value::Null]],
            values: vec![vec![Value::Null, Value::Null]],
            number_format: vec![vec![Value::Null, Value::Null]],
        };
        assert!(validate_shape(&raw, 1).is_err());
        assert!(validate_shape(&raw, 2).is_ok());
    }

    #[test]
    fn bridge_agent_rejects_an_invalid_ca_instead_of_disabling_tls_validation() {
        let config = BridgeConfig {
            base_url: "https://localhost:32123".into(),
            admin_token: "x".repeat(32),
            ca_cert_pem: b"not a certificate".to_vec(),
        };
        assert!(bridge_agent(&config).is_err());
    }
}
