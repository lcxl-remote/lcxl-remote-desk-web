//! Private Excel calculation protocol. Runs before server initialization in an
//! independent process, accepting one immutable package and no automation code.
mod dispatch;
mod native;
mod supervisor;
mod workspace;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
pub(crate) use supervisor::{available, calculate};

const MAX_FRAME: usize = 24 * 1024 * 1024;
const MAX_PACKAGE: usize = 16 * 1024 * 1024;
pub const MODE: &str = "office-xlsx-calculation-helper";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    invocation: String,
    sha256: String,
    package_base64: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u32,
    invocation: String,
    input_sha256: String,
    package_base64: String,
    results: Vec<CellResult>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CellResult {
    sheet: String,
    address: String,
    formula_digest: String,
    value: Scalar,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum Scalar {
    Number(f64),
    Boolean(bool),
    Text(String),
}

fn read_frame<T: serde::de::DeserializeOwned>(mut reader: impl Read) -> anyhow::Result<T> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header) as usize;
    anyhow::ensure!(
        size > 0 && size <= MAX_FRAME,
        "invalid Office helper frame size"
    );
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    let mut extra = [0];
    anyhow::ensure!(
        reader.read(&mut extra)? == 0,
        "extra Office helper protocol input"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
fn write_frame(mut writer: impl Write, value: &impl Serialize) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() <= MAX_FRAME,
        "Office helper response exceeds frame limit"
    );
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn run_stdio() -> i32 {
    let result = (|| -> anyhow::Result<()> {
        let request: Request = read_frame(std::io::stdin().lock())?;
        anyhow::ensure!(
            request.version == 1 && uuid::Uuid::parse_str(&request.invocation).is_ok(),
            "invalid helper invocation"
        );
        let bytes = base64::engine::general_purpose::STANDARD.decode(&request.package_base64)?;
        anyhow::ensure!(
            bytes.len() <= MAX_PACKAGE && format!("{:x}", Sha256::digest(&bytes)) == request.sha256,
            "helper package identity mismatch"
        );
        let (saved, results) = native::calculate(&bytes)?;
        write_frame(
            std::io::stdout().lock(),
            &Response {
                version: 1,
                invocation: request.invocation,
                input_sha256: request.sha256,
                package_base64: base64::engine::general_purpose::STANDARD.encode(saved),
                results,
            },
        )
    })();
    if let Err(cause) = result {
        // Never include document contents or an unbounded COM message on stdout.
        eprintln!("Excel helper failed: {cause}");
        1
    } else {
        0
    }
}
