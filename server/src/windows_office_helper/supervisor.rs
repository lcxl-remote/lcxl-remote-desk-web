//! One bounded helper invocation; never terminate applications by name.
use super::*;
use std::{
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
static RUNNING: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Helper {
    child: std::process::Child,
    finished: bool,
}
impl Drop for Helper {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            // Closing our Windows process handle does not leave a Unix-style
            // zombie. Do not turn a bounded timeout into an unbounded wait for
            // kernel I/O completion during termination. The private inflight
            // marker and helper-owned lock retain unresolved native state.
            let _ = self.child.try_wait();
        }
    }
}
pub(crate) fn available() -> bool {
    super::native::registered() && super::workspace::available().is_ok()
}

pub(crate) fn calculate(
    bytes: &[u8],
    still_authorized: impl Fn() -> anyhow::Result<()>,
) -> anyhow::Result<(
    Vec<u8>,
    Vec<desk_office_batch::xlsx_workbook_result::CalculationReadback>,
)> {
    #[cfg(not(test))]
    let executable = std::env::current_exe()?;
    // Explicit opt-in for ignored native integration tests only. Production
    // never consults an environment override for its executable.
    #[cfg(test)]
    let executable =
        std::path::PathBuf::from(std::env::var_os("LRD_TEST_SERVER_BINARY").ok_or_else(|| {
            anyhow::anyhow!("native tests require the production helper executable")
        })?);
    calculate_with_executable(bytes, still_authorized, &executable)
}

fn calculate_with_executable(
    bytes: &[u8],
    still_authorized: impl Fn() -> anyhow::Result<()>,
    executable: &std::path::Path,
) -> anyhow::Result<(
    Vec<u8>,
    Vec<desk_office_batch::xlsx_workbook_result::CalculationReadback>,
)> {
    let _exclusive = RUNNING
        .try_lock()
        .map_err(|_| anyhow::anyhow!("another Excel calculation is running"))?;
    // This is an early availability check. The helper acquires and retains the
    // cross-process user lock itself, closing the check-to-spawn race.
    super::workspace::available()?;
    desk_office_batch::xlsx_native_preflight::inspect(bytes)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    still_authorized()?;
    let invocation = uuid::Uuid::new_v4().to_string();
    let digest = format!("{:x}", Sha256::digest(bytes));
    let request = Request {
        version: 1,
        invocation: invocation.clone(),
        sha256: digest.clone(),
        package_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    let mut helper = Helper {
        child: Command::new(executable)
            .arg(MODE)
            .creation_flags(0x08000000) // CREATE_NO_WINDOW: helper has no interactive console.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
        finished: false,
    };
    let stdin = helper
        .child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("helper input pipe unavailable"))?;
    let stdout = helper
        .child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("helper output pipe unavailable"))?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = write_frame(stdin, &request).and_then(|()| read_frame::<Response>(stdout));
        let _ = tx.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    let response = loop {
        if Instant::now() >= deadline || still_authorized().is_err() {
            // This exact child was spawned above. Excel itself is never killed.
            anyhow::bail!("Excel calculation interrupted; native completion is unresolved");
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(value) => break value?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => anyhow::bail!("Excel helper transport failed"),
        }
    };
    // EOF proves the response pipe closed, not that the process exited. Keep
    // the same deadline and authorization checks while waiting for termination.
    let status = loop {
        if Instant::now() >= deadline || still_authorized().is_err() {
            anyhow::bail!("Excel helper shutdown interrupted; completion is unresolved");
        }
        if let Some(status) = helper.child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    helper.finished = true;
    anyhow::ensure!(
        status.success(),
        "Excel helper did not complete successfully"
    );
    still_authorized()?;
    anyhow::ensure!(
        response.version == 1
            && response.invocation == invocation
            && response.input_sha256 == digest,
        "Excel helper response is for another invocation"
    );
    let saved = base64::engine::general_purpose::STANDARD.decode(response.package_base64)?;
    anyhow::ensure!(
        saved.len() <= MAX_PACKAGE && response.results.len() <= 4096,
        "Excel result exceeds limits"
    );
    let expected = desk_office_batch::xlsx_formula_inventory::inspect(bytes)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let mut readback = Vec::with_capacity(response.results.len());
    for cell in response.results {
        anyhow::ensure!(
            expected.formulas.iter().any(|f| f.sheet == cell.sheet
                && f.address == cell.address
                && f.ast_digest_sha256 == cell.formula_digest),
            "native formula identity mismatch"
        );
        let value = match cell.value {
            Scalar::Number(number) => {
                anyhow::ensure!(number.is_finite(), "invalid native number");
                desk_office_batch::xlsx_result::Scalar::Number(number)
            }
            Scalar::Boolean(value) => desk_office_batch::xlsx_result::Scalar::Boolean(value),
            Scalar::Text(value) => {
                anyhow::ensure!(
                    value.len() <= 32 * 1024,
                    "native text result exceeds bounds"
                );
                desk_office_batch::xlsx_result::Scalar::Text(value)
            }
        };
        readback.push(
            desk_office_batch::xlsx_workbook_result::CalculationReadback {
                sheet: cell.sheet,
                address: cell.address,
                value,
            },
        );
    }
    let merged = desk_office_batch::xlsx_native_merge::merge(bytes, &saved, &readback)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    still_authorized()?;
    Ok((merged, readback))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    #[ignore = "requires production LRD_TEST_SERVER_BINARY, synthetic LRD_EXCEL_INPUT and new LRD_EXCEL_OUTPUT"]
    fn native_supervisor_rechecks_authority_and_preserves_source_package() {
        let executable =
            std::env::var_os("LRD_TEST_SERVER_BINARY").expect("production helper binary");
        let source = std::env::var_os("LRD_EXCEL_INPUT").expect("synthetic input workbook");
        let output = std::env::var_os("LRD_EXCEL_OUTPUT").expect("new output file");
        let input = std::fs::read(&source).unwrap();
        let checks = AtomicUsize::new(0);
        let (merged, readback) = calculate_with_executable(
            &input,
            || {
                checks.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            std::path::Path::new(&executable),
        )
        .unwrap();
        assert!(checks.load(Ordering::SeqCst) >= 4);
        assert_eq!(readback.len(), 3);
        desk_office_batch::xlsx_native_preservation::validate(&input, &merged).unwrap();
        desk_office_batch::xlsx_workbook_result::compare(&input, &merged, &readback).unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), input);
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(output)
            .unwrap();
        std::io::Write::write_all(&mut file, &merged).unwrap();
        file.sync_all().unwrap();
    }
}
