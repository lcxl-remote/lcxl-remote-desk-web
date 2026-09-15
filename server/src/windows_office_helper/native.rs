//! Fixed Excel automation on a fresh COM instance and private NTFS workspace.
use super::{CellResult, Scalar, dispatch::Dispatch};
use anyhow::{Context, ensure};
use desk_file_recovery::windows::{FileKind, PrivateDirectory, file_identity};
use std::{
    fs::OpenOptions,
    io::Read,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, HWND, WAIT_OBJECT_0},
        Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ},
        System::{
            Com::{
                CLSCTX_LOCAL_SERVER, CLSIDFromProgID, COINIT_APARTMENTTHREADED, CoCreateInstance,
                CoInitializeEx, CoUninitialize, IDispatch,
            },
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{
                GetCurrentProcess, GetCurrentProcessId, GetProcessId, GetProcessTimes, OpenProcess,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
            },
            Variant::{VARIANT, VT_BOOL, VT_BSTR, VT_I2, VT_I4, VT_R8},
        },
        UI::WindowsAndMessaging::GetWindowThreadProcessId,
    },
    core::{BSTR, w},
};

struct Apartment;
fn missing_argument() -> VARIANT {
    use windows::Win32::{
        Foundation::DISP_E_PARAMNOTFOUND,
        System::Variant::{VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ERROR},
    };
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_ERROR,
                Anonymous: VARIANT_0_0_0 {
                    scode: DISP_E_PARAMNOTFOUND.0,
                },
                ..Default::default()
            }),
        },
    }
}
pub(super) fn registered() -> bool {
    use windows::Win32::System::Registry::{HKEY_CLASSES_ROOT, RRF_RT_REG_SZ, RegGetValueW};
    let mut bytes = [0u8; 512];
    let mut size = bytes.len() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            w!("Excel.Application\\CLSID"),
            None,
            RRF_RT_REG_SZ,
            None,
            Some(bytes.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .is_ok()
        && size >= 4
        && size <= bytes.len() as u32
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
fn created(handle: HANDLE) -> anyhow::Result<u64> {
    let mut values = [FILETIME::default(); 4];
    let [a, b, c, d] = &mut values;
    unsafe {
        GetProcessTimes(handle, a, b, c, d)?;
    }
    Ok((u64::from(values[0].dwHighDateTime) << 32) | u64::from(values[0].dwLowDateTime))
}

pub(super) fn recorded_process_exited(
    record: &super::workspace::ProcessRecord,
) -> anyhow::Result<bool> {
    let process = match unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            record.pid,
        )
    } {
        Ok(handle) => Process(handle),
        // OpenProcess reports invalid parameter for a PID that no longer exists.
        Err(error) if error.code() == windows::Win32::Foundation::E_INVALIDARG => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    Ok(created(process.0)? != record.created
        || unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0)
}

struct OwnedApplication {
    app: Option<Dispatch>,
    _process: Process,
    private_input: PathBuf,
}
impl OwnedApplication {
    fn app(&self) -> &Dispatch {
        self.app
            .as_ref()
            .expect("owned Excel reference exists until shutdown")
    }
    fn create(input: PathBuf) -> anyhow::Result<Self> {
        let mut session = 0;
        unsafe {
            ProcessIdToSessionId(GetCurrentProcessId(), &mut session)?;
        }
        ensure!(session != 0, "Excel cannot run in the service session");
        let helper_created = created(unsafe { GetCurrentProcess() })?;
        let clsid = unsafe { CLSIDFromProgID(w!("Excel.Application"))? };
        let app = Dispatch(unsafe {
            CoCreateInstance::<_, IDispatch>(&clsid, None, CLSCTX_LOCAL_SERVER)?
        });
        let hwnd = HWND(app.integer("Hwnd")? as u32 as usize as *mut _);
        let mut pid = 0;
        ensure!(
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) } != 0 && pid != 0,
            "Excel process identity is unavailable"
        );
        let process = Process(unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )?
        });
        let mut excel_session = 0;
        unsafe {
            ProcessIdToSessionId(pid, &mut excel_session)?;
        }
        // Do not set policies or call Quit on a pre-existing application.
        ensure!(
            created(process.0)? > helper_created && excel_session == session,
            "Excel activation did not create a private process"
        );
        ensure!(
            !app.boolean("UserControl")? && !app.boolean("Visible")?,
            "Excel instance is user-controlled"
        );
        ensure!(
            app.object("Workbooks", vec![])?.integer("Count")? == 0,
            "Excel instance contains existing workbooks"
        );
        Ok(Self {
            app: Some(app),
            _process: process,
            private_input: input,
        })
    }
    fn policies(&self) -> anyhow::Result<()> {
        self.app().set_integer("AutomationSecurity", 3)?;
        for name in [
            "EnableEvents",
            "DisplayAlerts",
            "ScreenUpdating",
            "AskToUpdateLinks",
        ] {
            self.app().set_boolean(name, false)?;
        }
        self.app().set_boolean("IgnoreRemoteRequests", true)?;
        Ok(())
    }
    fn monitor(&self) -> anyhow::Result<Process> {
        let pid = unsafe { GetProcessId(self._process.0) };
        ensure!(pid != 0, "owned Excel process identity is unavailable");
        let process = Process(unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )?
        });
        ensure!(
            created(process.0)? == created(self._process.0)?,
            "Excel process identity changed"
        );
        Ok(process)
    }

    fn finish(&mut self) -> anyhow::Result<()> {
        ensure!(
            !self.app().boolean("UserControl")?,
            "Excel became user-controlled"
        );
        ensure!(
            self.app().object("Workbooks", vec![])?.integer("Count")? == 0,
            "Excel contains another workbook"
        );
        self.app().call("Quit", vec![])?;
        drop(self.app.take());
        ensure!(
            unsafe { WaitForSingleObject(self._process.0, 5000) } == WAIT_OBJECT_0,
            "Excel process shutdown is unresolved"
        );
        Ok(())
    }
}
impl Drop for OwnedApplication {
    fn drop(&mut self) {
        if self.app.is_none() {
            return;
        }
        // Never close an unrelated workbook, even if it appeared in this new
        // instance after activation. Only our exact private input is disposable.
        let _ = (|| -> anyhow::Result<()> {
            ensure!(
                !self.app().boolean("UserControl")?,
                "Excel became user-controlled"
            );
            let books = self.app().object("Workbooks", vec![])?;
            if books.integer("Count")? == 1 {
                let book = books.object("Item", vec![1i32.into()])?;
                ensure!(
                    Path::new(&book.text("FullName")?) == self.private_input,
                    "unexpected Excel workbook"
                );
                book.call("Close", vec![false.into()])?;
            }
            ensure!(
                books.integer("Count")? == 0,
                "Excel contains another workbook"
            );
            self.app().call("Quit", vec![])?;
            Ok(())
        })();
    }
}

pub(super) fn calculate(bytes: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<CellResult>)> {
    let inventory = desk_office_batch::xlsx_native_preflight::inspect(bytes)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    // Reject a missing dependency before creating an unresolved-activation
    // marker. Once activation starts, failures still require process proof.
    ensure!(
        registered(),
        "Excel is not registered for this Windows user"
    );
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }
    let _apartment = Apartment;
    let (root_path, root) = super::workspace::open()?;
    // The helper owns the lock so it survives a supervisor crash until this
    // native invocation exits. Activation never precedes this exclusion check.
    let _user_lock = root
        .try_lock()?
        .context("another user-session Excel calculation is running")?;
    super::workspace::ensure_no_inflight(&root_path, &root)?;
    let id = uuid::Uuid::new_v4().to_string();
    let workspace = PrivateDirectory::create_new(root.handle(), &id)?;
    let lock = workspace.lock()?;
    let directory = root_path.join(&id);
    let input = directory.join("input.xlsx");
    let saved = directory.join("calculated.xlsx");
    lock.create_new("input.xlsx", bytes)?;
    lock.create_new(
        "inflight",
        b"Excel activation or shutdown is not yet confirmed",
    )?;
    let mut monitor = None;
    let result = (|| -> anyhow::Result<_> {
        let mut owned = OwnedApplication::create(input.clone())?;
        monitor = Some(owned.monitor()?);
        lock.create_new(
            "process.json",
            &serde_json::to_vec(&super::workspace::ProcessRecord {
                version: 1,
                pid: unsafe { GetProcessId(owned._process.0) },
                created: created(owned._process.0)?,
            })?,
        )?;
        owned.policies()?;
        let books = owned.app().object("Workbooks", vec![])?;
        let input_text = input.to_str().context("private input path encoding")?;
        // Positional Workbooks.Open: no link refresh, read-only, explicit empty
        // passwords, no notifications/MRU, no repair or format conversion.
        let book = Dispatch::from_variant(&books.call(
            "Open",
            vec![
                input_text.into(),
                0i32.into(),
                true.into(),
                missing_argument(),
                "".into(),
                "".into(),
                true.into(),
                missing_argument(),
                missing_argument(),
                false.into(),
                false.into(),
                missing_argument(),
                false.into(),
                false.into(),
                0i32.into(),
            ],
        )?)?;
        ensure!(
            books.integer("Count")? == 1 && Path::new(&book.text("FullName")?) == input,
            "Excel opened an unexpected workbook"
        );
        ensure!(
            owned.app().integer("AutomationSecurity")? == 3
                && !owned.app().boolean("EnableEvents")?,
            "Excel open policy changed"
        );
        owned.app().call("CalculateFullRebuild", vec![])?;
        ensure!(
            owned.app().integer("CalculationState")? == 0,
            "Excel calculation did not complete"
        );
        let sheets = book.object("Worksheets", vec![])?;
        ensure!(
            sheets.integer("Count")? as usize == inventory.worksheet_count,
            "Excel worksheet count changed"
        );
        for (index, name) in inventory.worksheet_names.iter().enumerate() {
            ensure!(
                sheets
                    .object("Item", vec![((index + 1) as i32).into()])?
                    .text("Name")?
                    == *name,
                "Excel worksheet names or order changed"
            );
        }
        let mut results = Vec::with_capacity(inventory.formulas.len());
        let mut result_bytes = 0usize;
        for formula in &inventory.formulas {
            let sheet = sheets.object("Item", vec![formula.sheet.as_str().into()])?;
            ensure!(
                sheet.text("Name")? == formula.sheet,
                "Excel worksheet identity changed"
            );
            let cell = sheet.object("Range", vec![formula.address.as_str().into()])?;
            let native_formula = cell.text("Formula")?;
            let proof = desk_diagnose_core::spreadsheet_formula::validate_formula_patch(
                &native_formula,
                &formula.address,
                desk_diagnose_core::spreadsheet_formula::FORMULA_LOCALE_V1,
                &inventory.worksheet_names,
            )
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            ensure!(
                proof.ast_digest_sha256 == formula.ast_digest_sha256,
                "Excel changed the formula"
            );
            let value = cell.get("Value2")?;
            let value = match value.vt() {
                VT_BOOL => Scalar::Boolean(bool::try_from(&value)?),
                VT_BSTR => {
                    let text = BSTR::try_from(&value)?.to_string();
                    ensure!(text.len() <= 32 * 1024, "Excel text result exceeds bounds");
                    result_bytes += text.len();
                    Scalar::Text(text)
                }
                VT_R8 | VT_I2 | VT_I4 => {
                    let number = f64::try_from(&value)?;
                    ensure!(number.is_finite(), "Excel produced a nonfinite result");
                    Scalar::Number(number)
                }
                _ => anyhow::bail!("Excel produced an unsupported or error result"),
            };
            result_bytes += formula.sheet.len() + formula.address.len() + 160;
            ensure!(
                result_bytes <= 1024 * 1024,
                "Excel calculation readback exceeds bounds"
            );
            results.push(CellResult {
                sheet: formula.sheet.clone(),
                address: formula.address.clone(),
                formula_digest: formula.ast_digest_sha256.clone(),
                value,
            });
        }
        ensure!(
            owned.app().integer("CalculationState")? == 0 && books.integer("Count")? == 1,
            "Excel changed during readback"
        );
        ensure!(!saved.try_exists()?, "private output already exists");
        book.call(
            "SaveCopyAs",
            vec![
                saved
                    .to_str()
                    .context("private output path encoding")?
                    .into(),
            ],
        )?;
        // Release child COM references before waiting for the owned Excel process.
        drop(sheets);
        book.call("Close", vec![false.into()])?;
        drop(book);
        ensure!(
            books.integer("Count")? == 0,
            "Excel did not close the private workbook"
        );
        drop(books);
        owned.finish()?;
        drop(owned);
        std::fs::remove_file(directory.join("inflight"))?;
        ensure!(
            lock.read("input.xlsx", super::MAX_PACKAGE as u64)? == bytes,
            "Excel changed the private source"
        );
        workspace.validate()?;
        let mut handle = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&saved)?;
        let identity = file_identity(&handle, FileKind::File)?;
        let mut output = Vec::new();
        (&mut handle)
            .take(super::MAX_PACKAGE as u64 + 1)
            .read_to_end(&mut output)?;
        ensure!(
            output.len() <= super::MAX_PACKAGE
                && file_identity(&handle, FileKind::File)? == identity,
            "native output identity or size changed"
        );
        workspace.validate()?;
        Ok((output, results))
    })();
    // All automation references from the closure have been dropped. A failed
    // calculation may still have closed its own application cleanly; only this
    // held process identity can establish that there is no live native work.
    if result.is_err()
        && monitor.as_ref().is_some_and(|process| {
            (unsafe { WaitForSingleObject(process.0, 5000) }) == WAIT_OBJECT_0
        })
    {
        let _ = std::fs::remove_file(directory.join("inflight"));
    }
    // Nonrecursive cleanup of fixed leaves in the uniquely created workspace.
    // Ancestor handles remain pinned; failure never changes the task outcome.
    drop(lock);
    if !directory.join("inflight").try_exists().unwrap_or(true) {
        let _ = super::workspace::remove_fixed_files(&directory);
    }
    drop(workspace);
    let _ = std::fs::remove_dir(&directory);
    result
}
