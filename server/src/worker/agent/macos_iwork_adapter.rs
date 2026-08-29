//! Closed-surface macOS iWork adapter backed directly by ScriptingBridge.
//!
//! The model-facing protocol contains only typed document mutations. Bundle
//! identifiers, selectors and scripting dictionary contracts are compiled into
//! this module; no caller can supply Apple Event codes or source text.

#![allow(unsafe_op_in_unsafe_fn)]

use std::ffi::{CStr, CString, c_char};
use std::fs;
use std::panic::UnwindSafe;
use std::path::Path;

use desk_agent_protocol::computer_use::{
    DocumentLivePatchAction, PresentationLivePatchAction, SpreadsheetLivePatchAction,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send, sel};
use sha2::{Digest, Sha256};
use sysinfo::System;

pub const IWORK_ADAPTER_VERSION: &str = "iwork-scripting-bridge/1";
const APPLE_EVENT_TIMEOUT_TICKS: i64 = 15 * 60;
const APPLE_EVENT_TIMEOUT_SECONDS: f64 = 15.0;
const CORE_EVENT_CLASS: u32 = fourcc(*b"aevt");
const OPEN_DOCUMENTS_EVENT: u32 = fourcc(*b"odoc");
const KEY_DIRECT_OBJECT: u32 = fourcc(*b"----");
const APPLE_EVENT_DEFAULT_SEND_OPTIONS: usize = 0x0000_0023;
const CORE_SUITE: u32 = fourcc(*b"core");
const CLOSE_EVENT: u32 = fourcc(*b"clos");
const SAVE_EVENT: u32 = fourcc(*b"save");
const SAVE_OPTIONS_PARAMETER: u32 = fourcc(*b"savo");
const FILE_PARAMETER: u32 = fourcc(*b"kfil");
const FILE_TYPE_PARAMETER: u32 = fourcc(*b"fltp");
const EXPORT_DESTINATION_PARAMETER: u32 = fourcc(*b"pfil");
const EXPORT_FORMAT_PARAMETER: u32 = fourcc(*b"exft");
const EXPORT_PROPERTIES_PARAMETER: u32 = fourcc(*b"expr");
const DOCUMENTS: u32 = fourcc(*b"docu");
const TABLES: u32 = fourcc(*b"NmTb");
const CELLS: u32 = fourcc(*b"NmCl");
const VERSION: u32 = fourcc(*b"vers");
const NAME: u32 = fourcc(*b"pnam");
const FILE: u32 = fourcc(*b"file");
const ACTIVE_SHEET: u32 = fourcc(*b"NmAS");
const SELECTION_RANGE: u32 = fourcc(*b"NMTs");
const CELL_VALUE: u32 = fourcc(*b"NMCv");
const CELL_FORMULA: u32 = fourcc(*b"NMCf");
const FORMATTED_VALUE: u32 = fourcc(*b"NMfv");
const BODY_TEXT: u32 = fourcc(*b"pTxt");
const CURRENT_SLIDE: u32 = fourcc(*b"crsl");
const SLIDE_NUMBER: u32 = fourcc(*b"KSdN");
const DEFAULT_TITLE_ITEM: u32 = fourcc(*b"sdti");
const OBJECT_TEXT: u32 = fourcc(*b"pDTx");
const PRESENTER_NOTES: u32 = fourcc(*b"ksnt");
const SAVE_NO: u32 = fourcc(*b"no  ");
const NUMBERS_NATIVE_FORMAT: u32 = fourcc(*b"Nuff");
const PAGES_NATIVE_FORMAT: u32 = fourcc(*b"Pgff");
const KEYNOTE_NATIVE_FORMAT: u32 = fourcc(*b"Knff");
const NUMBERS_PDF_FORMAT: u32 = fourcc(*b"Npdf");
const PAGES_PDF_FORMAT: u32 = fourcc(*b"Ppdf");
const KEYNOTE_PDF_FORMAT: u32 = fourcc(*b"Kpdf");
const NUMBERS_MICROSOFT_OFFICE_FORMAT: u32 = fourcc(*b"Nexl");
const PAGES_MICROSOFT_OFFICE_FORMAT: u32 = fourcc(*b"Pwrd");
const KEYNOTE_MICROSOFT_OFFICE_FORMAT: u32 = fourcc(*b"Kppt");

#[link(name = "ScriptingBridge", kind = "framework")]
unsafe extern "C" {}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

unsafe extern "C-unwind" {
    #[link_name = "objc_msgSend"]
    fn objc_msg_send_variadic(
        receiver: *mut AnyObject,
        selector: objc2::runtime::Sel,
        event_class: u32,
        event_id: u32,
        first_parameter_code: u32,
        ...
    ) -> *mut AnyObject;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IworkApplication {
    Numbers,
    Pages,
    Keynote,
}

#[derive(Clone, Copy)]
struct AppSpec {
    app: IworkApplication,
    bundle_id: &'static str,
    executable_path: &'static str,
    version: &'static str,
    sdef_path: &'static str,
    sdef_sha256: &'static str,
}

const APP_SPECS: [AppSpec; 3] = [
    AppSpec {
        app: IworkApplication::Numbers,
        bundle_id: "com.apple.Numbers",
        executable_path: "/Applications/Numbers Creator Studio.app/Contents/MacOS/Numbers",
        version: "15.3.1",
        sdef_path: "/Applications/Numbers Creator Studio.app/Contents/Resources/Numbers.sdef",
        sdef_sha256: "ee098803afeec84afa58bc37bff97115eb5c5bac07d14d9fbff27ee16e3e1d9b",
    },
    AppSpec {
        app: IworkApplication::Pages,
        bundle_id: "com.apple.Pages",
        executable_path: "/Applications/Pages Creator Studio.app/Contents/MacOS/Pages",
        version: "15.3.1",
        sdef_path: "/Applications/Pages Creator Studio.app/Contents/Resources/Pages.sdef",
        sdef_sha256: "412e6fb4959ad8486e4d8ce0a5ce93de22a3cdbfb460e7fd21d87000ed860fb3",
    },
    AppSpec {
        app: IworkApplication::Keynote,
        bundle_id: "com.apple.Keynote",
        executable_path: "/Applications/Keynote Creator Studio.app/Contents/MacOS/Keynote",
        version: "15.3.1",
        sdef_path: "/Applications/Keynote Creator Studio.app/Contents/Resources/Keynote.sdef",
        sdef_sha256: "62b866e5db36a815d18e64841ce6ead3ca78f1231c76e5aa3b0d7a01d4b9408e",
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IworkAppReadiness {
    pub application: IworkApplication,
    pub bundle_id: &'static str,
    pub expected_version: &'static str,
    pub running: bool,
    pub contract_verified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumbersCellLocator {
    pub document_identity_sha256: String,
    pub sheet_name: String,
    pub table_name: String,
    pub cell_address: String,
    pub before_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PagesDocumentLocator {
    pub document_identity_sha256: String,
    pub before_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeynoteSlideLocator {
    pub document_identity_sha256: String,
    pub slide_number: i64,
    pub title_before_sha256: String,
    pub notes_before_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IworkObservation {
    Numbers {
        locator: NumbersCellLocator,
        value: String,
        formula: Option<String>,
        formatted_value: String,
    },
    Pages {
        locator: PagesDocumentLocator,
        body_text: String,
    },
    Keynote {
        locator: KeynoteSlideLocator,
        title: String,
        presenter_notes: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IworkMutationResult {
    pub changed: bool,
    pub verified: bool,
    pub summary: String,
    pub readback_sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IworkBatchExportFormat {
    Pdf,
    MicrosoftOffice,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IworkBatchOutput<'a> {
    pub native_path: &'a Path,
    pub export: Option<(IworkBatchExportFormat, &'a Path)>,
}

pub fn readiness() -> Vec<IworkAppReadiness> {
    APP_SPECS
        .iter()
        .map(|spec| IworkAppReadiness {
            application: spec.app,
            bundle_id: spec.bundle_id,
            expected_version: spec.version,
            running: application_is_running(spec.bundle_id).unwrap_or(false),
            contract_verified: verify_contract(*spec).is_ok(),
        })
        .collect()
}

pub fn observe(application: IworkApplication) -> Result<IworkObservation, AgentError> {
    let spec = spec(application);
    verify_contract(spec)?;
    ensure_automation_permission(spec)?;
    with_bridge(|| unsafe {
        let app = running_application(spec)?;
        verify_app_version(app, spec)?;
        let document = front_document(app)?;
        match application {
            IworkApplication::Numbers => observe_numbers_document(document),
            IworkApplication::Pages => observe_pages_document(document),
            IworkApplication::Keynote => observe_keynote_document(document),
        }
    })?
}

/// Open one caller-verified file through the frozen iWork scripting contract,
/// project its bounded semantic object, then close it without saving. This is
/// BatchDocument inspection: it does not use the front document or selection
/// of an unrelated open file, and it never writes the selected source.
pub fn observe_batch(
    application: IworkApplication,
    source_path: &Path,
) -> Result<IworkObservation, AgentError> {
    let spec = spec(application);
    verify_contract(spec)?;
    ensure_automation_permission(spec)?;
    let source_path = source_path.to_path_buf();
    with_bridge(|| unsafe {
        let app = running_application(spec)?;
        verify_app_version(app, spec)?;
        let document = open_document(app, spec, &source_path)?;
        let observation = match application {
            IworkApplication::Numbers => observe_numbers_document(document),
            IworkApplication::Pages => observe_pages_document(document),
            IworkApplication::Keynote => observe_keynote_document(document),
        };
        let closed = close_without_saving(document);
        match (observation, closed) {
            (Ok(observation), Ok(())) => Ok(observation),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    })?
}

pub fn apply_numbers(
    locator: &NumbersCellLocator,
    action: &SpreadsheetLivePatchAction,
) -> Result<IworkMutationResult, AgentError> {
    let spec = spec(IworkApplication::Numbers);
    verify_contract(spec)?;
    ensure_automation_permission(spec)?;
    let locator = locator.clone();
    let action = action.clone();
    with_bridge(|| unsafe {
        let app = running_application(spec)?;
        verify_app_version(app, spec)?;
        let document = front_document(app)?;
        apply_numbers_document(document, &locator, &action)
    })?
}

pub fn apply_pages(
    locator: &PagesDocumentLocator,
    action: &DocumentLivePatchAction,
) -> Result<IworkMutationResult, AgentError> {
    let spec = spec(IworkApplication::Pages);
    verify_contract(spec)?;
    ensure_automation_permission(spec)?;
    let locator = locator.clone();
    let action = action.clone();
    with_bridge(|| unsafe {
        let app = running_application(spec)?;
        verify_app_version(app, spec)?;
        let document = front_document(app)?;
        apply_pages_document(document, &locator, &action)
    })?
}

pub fn apply_keynote(
    locator: &KeynoteSlideLocator,
    action: &PresentationLivePatchAction,
) -> Result<IworkMutationResult, AgentError> {
    let spec = spec(IworkApplication::Keynote);
    verify_contract(spec)?;
    ensure_automation_permission(spec)?;
    let locator = locator.clone();
    let action = action.clone();
    with_bridge(|| unsafe {
        let app = running_application(spec)?;
        verify_app_version(app, spec)?;
        let document = front_document(app)?;
        apply_keynote_document(document, &locator, &action)
    })?
}

pub fn apply_numbers_batch(
    source_path: &Path,
    output: &IworkBatchOutput<'_>,
    locator: &NumbersCellLocator,
    action: &SpreadsheetLivePatchAction,
) -> Result<IworkMutationResult, AgentError> {
    apply_batch(
        IworkApplication::Numbers,
        source_path,
        output,
        &locator.document_identity_sha256,
        |document| unsafe {
            let mut locator = locator.clone();
            locator.document_identity_sha256 = document_identity(document)?;
            apply_numbers_document(document, &locator, action)
        },
    )
}

pub fn apply_pages_batch(
    source_path: &Path,
    output: &IworkBatchOutput<'_>,
    locator: &PagesDocumentLocator,
    action: &DocumentLivePatchAction,
) -> Result<IworkMutationResult, AgentError> {
    apply_batch(
        IworkApplication::Pages,
        source_path,
        output,
        &locator.document_identity_sha256,
        |document| unsafe {
            let mut locator = locator.clone();
            locator.document_identity_sha256 = document_identity(document)?;
            apply_pages_document(document, &locator, action)
        },
    )
}

pub fn apply_keynote_batch(
    source_path: &Path,
    output: &IworkBatchOutput<'_>,
    locator: &KeynoteSlideLocator,
    action: &PresentationLivePatchAction,
) -> Result<IworkMutationResult, AgentError> {
    apply_batch(
        IworkApplication::Keynote,
        source_path,
        output,
        &locator.document_identity_sha256,
        |document| unsafe {
            let mut locator = locator.clone();
            locator.document_identity_sha256 = document_identity(document)?;
            apply_keynote_document(document, &locator, action)
        },
    )
}

fn apply_batch(
    application: IworkApplication,
    source_path: &Path,
    output: &IworkBatchOutput<'_>,
    expected_document_identity_sha256: &str,
    mutate: impl FnOnce(*mut AnyObject) -> Result<IworkMutationResult, AgentError> + UnwindSafe,
) -> Result<IworkMutationResult, AgentError> {
    let spec = spec(application);
    verify_contract(spec)?;
    ensure_automation_permission(spec)?;
    let source_path = source_path.to_path_buf();
    let output = output.clone();
    with_bridge(|| unsafe {
        let app = running_application(spec)?;
        verify_app_version(app, spec)?;
        let document = open_document(app, spec, &source_path)?;
        if document_identity(document)? != expected_document_identity_sha256 {
            let _ = close_without_saving(document);
            return Err(stale("the iWork batch document changed after observation"));
        }
        if let Err(error) = save_document(document, output.native_path, native_format(application))
        {
            let _ = close_without_saving(document);
            return Err(error);
        }
        let mutation = mutate(document);
        let Ok(mut mutation) = mutation else {
            let _ = close_without_saving(document);
            return mutation;
        };
        if let Err(error) = save_document(document, output.native_path, native_format(application))
        {
            let _ = close_without_saving(document);
            return Err(error);
        }
        if let Some((format, path)) = output.export
            && let Err(error) = export_document(
                document,
                application,
                path,
                export_format(application, format),
            )
        {
            let _ = close_without_saving(document);
            mutation.verified = false;
            mutation.summary = format!(
                "{}; native copy was saved but export verification failed: {}",
                mutation.summary, error.message
            );
            return Ok(mutation);
        }
        if let Err(error) = close_without_saving(document) {
            mutation.verified = false;
            mutation.summary = format!(
                "{}; output was saved but the batch document could not be closed: {}",
                mutation.summary, error.message
            );
        }
        Ok(mutation)
    })?
}

unsafe fn apply_numbers_document(
    document: *mut AnyObject,
    locator: &NumbersCellLocator,
    action: &SpreadsheetLivePatchAction,
) -> Result<IworkMutationResult, AgentError> {
    let observed = observe_numbers_document(document)?;
    let IworkObservation::Numbers {
        locator: current,
        value: before_value,
        formula: before_formula,
        ..
    } = observed
    else {
        unreachable!();
    };
    if current != *locator {
        return Err(stale("the Numbers cell changed after observation"));
    }
    let cell = numbers_cell(document)?;
    let requested = match action {
        SpreadsheetLivePatchAction::SetCellValue { value } => value,
        // Numbers exposes `formula` as read-only. Its scripting contract requires
        // formula text to be assigned through the writable `value` property.
        SpreadsheetLivePatchAction::SetCellFormula { formula } => formula,
    };
    set_property_string(
        cell,
        CELL_VALUE,
        requested,
        "Numbers rejected the cell mutation",
    )?;
    let after_value = optional_property_string(cell, CELL_VALUE)?.unwrap_or_default();
    let after_formula =
        optional_property_string(cell, CELL_FORMULA)?.filter(|value| !value.is_empty());
    let exact = match action {
        SpreadsheetLivePatchAction::SetCellValue { value } => after_value == *value,
        SpreadsheetLivePatchAction::SetCellFormula { formula } => {
            after_formula.as_deref() == Some(formula.as_str())
        }
    };
    let changed = before_value != after_value || before_formula != after_formula;
    Ok(IworkMutationResult {
        changed,
        verified: exact,
        summary: if exact {
            "Numbers cell mutation matched exact semantic read-back".into()
        } else if changed {
            "Numbers changed the cell but normalized or obscured the semantic read-back".into()
        } else {
            "Numbers cell remained unchanged".into()
        },
        readback_sha256: sha256_pair(&after_value, after_formula.as_deref().unwrap_or("")),
    })
}

unsafe fn apply_pages_document(
    document: *mut AnyObject,
    locator: &PagesDocumentLocator,
    action: &DocumentLivePatchAction,
) -> Result<IworkMutationResult, AgentError> {
    let observed = observe_pages_document(document)?;
    let IworkObservation::Pages {
        locator: current,
        body_text: before,
    } = observed
    else {
        unreachable!();
    };
    if current != *locator {
        return Err(stale("the Pages document changed after observation"));
    }
    let DocumentLivePatchAction::ReplaceBodyText { text } = action;
    set_property_string(
        document,
        BODY_TEXT,
        text,
        "Pages rejected the body mutation",
    )?;
    let after = property_string(document, BODY_TEXT)?;
    Ok(exact_text_result("Pages body", &before, &after, text))
}

unsafe fn apply_keynote_document(
    document: *mut AnyObject,
    locator: &KeynoteSlideLocator,
    action: &PresentationLivePatchAction,
) -> Result<IworkMutationResult, AgentError> {
    let observed = observe_keynote_document(document)?;
    let IworkObservation::Keynote {
        locator: current,
        title: before_title,
        presenter_notes: before_notes,
    } = observed
    else {
        unreachable!();
    };
    if current != *locator {
        return Err(stale("the Keynote slide changed after observation"));
    }
    let slide = current_keynote_slide(document)?;
    let (label, before, expected, readback) = match action {
        PresentationLivePatchAction::ReplaceSlideTitle { text } => {
            let title = property_object(slide, DEFAULT_TITLE_ITEM)?;
            if title.is_null() {
                return Err(failure(
                    AgentErrorKind::InvalidInput,
                    "the current Keynote slide has no default title item",
                    false,
                ));
            }
            set_property_string(
                title,
                OBJECT_TEXT,
                text,
                "Keynote rejected the title mutation",
            )?;
            (
                "Keynote title",
                before_title,
                text,
                property_string(title, OBJECT_TEXT)?,
            )
        }
        PresentationLivePatchAction::SetPresenterNotes { text } => {
            set_property_string(
                slide,
                PRESENTER_NOTES,
                text,
                "Keynote rejected the notes mutation",
            )?;
            (
                "Keynote presenter notes",
                before_notes,
                text,
                property_string(slide, PRESENTER_NOTES)?,
            )
        }
    };
    Ok(exact_text_result(label, &before, &readback, expected))
}

fn exact_text_result(
    label: &str,
    before: &str,
    after: &str,
    expected: &str,
) -> IworkMutationResult {
    let verified = after == expected;
    let changed = before != after;
    IworkMutationResult {
        changed,
        verified,
        summary: if verified {
            format!("{label} matched exact semantic read-back")
        } else if changed {
            format!("{label} changed but did not match exact semantic read-back")
        } else {
            format!("{label} remained unchanged")
        },
        readback_sha256: sha256(after.as_bytes()),
    }
}

unsafe fn observe_numbers_document(
    document: *mut AnyObject,
) -> Result<IworkObservation, AgentError> {
    let document_identity_sha256 = document_identity(document)?;
    let sheet = property_object(document, ACTIVE_SHEET)?;
    if sheet.is_null() {
        return Err(unavailable("Numbers has no active sheet"));
    }
    let sheet_name = property_string(sheet, NAME)?;
    let tables = element_array(sheet, TABLES)?;
    let table = first_element(tables, "Numbers active sheet has no table")?;
    let table_name = property_string(table, NAME)?;
    let range = property_object(table, SELECTION_RANGE)?;
    if range.is_null() {
        return Err(unavailable("Numbers has no selected cell range"));
    }
    let cells = element_array(range, CELLS)?;
    let cell = first_element(cells, "Numbers selection contains no cell")?;
    let cell_address = property_string(cell, NAME)?;
    let value = optional_property_string(cell, CELL_VALUE)?.unwrap_or_default();
    let formula = optional_property_string(cell, CELL_FORMULA)?.filter(|value| !value.is_empty());
    let formatted_value = optional_property_string(cell, FORMATTED_VALUE)?.unwrap_or_default();
    let before_sha256 = sha256_pair(&value, formula.as_deref().unwrap_or(""));
    Ok(IworkObservation::Numbers {
        locator: NumbersCellLocator {
            document_identity_sha256,
            sheet_name,
            table_name,
            cell_address,
            before_sha256,
        },
        value,
        formula,
        formatted_value,
    })
}

unsafe fn numbers_cell(document: *mut AnyObject) -> Result<*mut AnyObject, AgentError> {
    let sheet = property_object(document, ACTIVE_SHEET)?;
    if sheet.is_null() {
        return Err(unavailable("Numbers has no active sheet"));
    }
    let tables = element_array(sheet, TABLES)?;
    let table = first_element(tables, "Numbers active sheet has no table")?;
    let range = property_object(table, SELECTION_RANGE)?;
    if range.is_null() {
        return Err(unavailable("Numbers has no selected cell range"));
    }
    let cells = element_array(range, CELLS)?;
    first_element(cells, "Numbers selection contains no cell")
}

unsafe fn observe_pages_document(document: *mut AnyObject) -> Result<IworkObservation, AgentError> {
    let body_text = property_string(document, BODY_TEXT)?;
    Ok(IworkObservation::Pages {
        locator: PagesDocumentLocator {
            document_identity_sha256: document_identity(document)?,
            before_sha256: sha256(body_text.as_bytes()),
        },
        body_text,
    })
}

unsafe fn observe_keynote_document(
    document: *mut AnyObject,
) -> Result<IworkObservation, AgentError> {
    let slide = current_keynote_slide(document)?;
    let slide_number = property_string(slide, SLIDE_NUMBER)?
        .parse::<i64>()
        .map_err(|_| {
            failure(
                AgentErrorKind::TransportError,
                "Keynote returned an invalid slide number",
                true,
            )
        })?;
    let title = property_object(slide, DEFAULT_TITLE_ITEM)?;
    let title = if title.is_null() {
        String::new()
    } else {
        property_string(title, OBJECT_TEXT)?
    };
    let presenter_notes = optional_property_string(slide, PRESENTER_NOTES)?.unwrap_or_default();
    Ok(IworkObservation::Keynote {
        locator: KeynoteSlideLocator {
            document_identity_sha256: document_identity(document)?,
            slide_number,
            title_before_sha256: sha256(title.as_bytes()),
            notes_before_sha256: sha256(presenter_notes.as_bytes()),
        },
        title,
        presenter_notes,
    })
}

unsafe fn current_keynote_slide(document: *mut AnyObject) -> Result<*mut AnyObject, AgentError> {
    let slide = property_object(document, CURRENT_SLIDE)?;
    if slide.is_null() {
        Err(unavailable("Keynote has no current slide"))
    } else {
        Ok(slide)
    }
}

unsafe fn running_application(spec: AppSpec) -> Result<*mut AnyObject, AgentError> {
    running_process_id(spec).ok_or_else(|| {
        failure(
            AgentErrorKind::TargetOffline,
            "the iWork application is not running",
            true,
        )
    })?;
    let bundle_path = Path::new(spec.executable_path)
        .ancestors()
        .nth(3)
        .ok_or_else(|| failure(AgentErrorKind::Internal, "invalid frozen iWork path", false))?;
    let bundle_url = file_url(bundle_path)?;
    let app: *mut AnyObject = msg_send![
        class!(SBApplication),
        applicationWithURL: bundle_url
    ];
    if app.is_null() {
        return Err(unavailable("the iWork application cannot be resolved"));
    }
    let _: () = msg_send![app, setTimeout: APPLE_EVENT_TIMEOUT_TICKS];
    Ok(app)
}

unsafe fn file_url(path: &Path) -> Result<*mut AnyObject, AgentError> {
    let path = path.to_str().ok_or_else(|| {
        failure(
            AgentErrorKind::InvalidInput,
            "iWork batch paths must be valid UTF-8",
            false,
        )
    })?;
    let path = nsstring(path)?;
    let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: path];
    if url.is_null() {
        Err(failure(
            AgentErrorKind::Internal,
            "cannot construct an iWork batch file URL",
            true,
        ))
    } else {
        Ok(url)
    }
}

unsafe fn open_document(
    app: *mut AnyObject,
    spec: AppSpec,
    path: &Path,
) -> Result<*mut AnyObject, AgentError> {
    let url = file_url(path)?;
    let bundle_id = nsstring(spec.bundle_id)?;
    let target: *mut AnyObject = msg_send![
        class!(NSAppleEventDescriptor),
        descriptorWithBundleIdentifier: bundle_id
    ];
    let file: *mut AnyObject =
        msg_send![class!(NSAppleEventDescriptor), descriptorWithFileURL: url];
    let files: *mut AnyObject = msg_send![class!(NSAppleEventDescriptor), listDescriptor];
    if target.is_null() || file.is_null() || files.is_null() {
        return Err(failure(
            AgentErrorKind::Internal,
            "cannot construct the iWork open-document Apple event",
            true,
        ));
    }
    let _: () = msg_send![files, insertDescriptor: file atIndex: 1isize];
    let event: *mut AnyObject = msg_send![
        class!(NSAppleEventDescriptor),
        appleEventWithEventClass: CORE_EVENT_CLASS
        eventID: OPEN_DOCUMENTS_EVENT
        targetDescriptor: target
        returnID: -1i16
        transactionID: 0i32
    ];
    if event.is_null() {
        return Err(failure(
            AgentErrorKind::Internal,
            "cannot construct the iWork open-document Apple event",
            true,
        ));
    }
    let _: () = msg_send![event, setParamDescriptor: files forKeyword: KEY_DIRECT_OBJECT];
    let mut error: *mut AnyObject = std::ptr::null_mut();
    let reply: *mut AnyObject = msg_send![
        event,
        sendEventWithOptions: APPLE_EVENT_DEFAULT_SEND_OPTIONS
        timeout: APPLE_EVENT_TIMEOUT_SECONDS
        error: &mut error
    ];
    if reply.is_null() || !error.is_null() {
        Err(failure(
            AgentErrorKind::TransportError,
            "iWork rejected the selected batch document open request",
            true,
        ))
    } else {
        front_document(app)
    }
}

unsafe fn save_document(
    document: *mut AnyObject,
    path: &Path,
    format: u32,
) -> Result<(), AgentError> {
    let url = file_url(path)?;
    let format = enum_descriptor(format)?;
    send_event_two(
        document,
        CORE_SUITE,
        SAVE_EVENT,
        FILE_PARAMETER,
        url,
        FILE_TYPE_PARAMETER,
        format,
    );
    if path.is_file() {
        Ok(())
    } else {
        Err(failure(
            AgentErrorKind::TransportError,
            "iWork did not create the requested native batch copy",
            true,
        ))
    }
}

unsafe fn export_document(
    document: *mut AnyObject,
    application: IworkApplication,
    path: &Path,
    format: u32,
) -> Result<(), AgentError> {
    let url = file_url(path)?;
    let format = enum_descriptor(format)?;
    let (event_class, event_id) = match application {
        IworkApplication::Numbers => (fourcc(*b"Nmst"), fourcc(*b"expo")),
        IworkApplication::Pages => (fourcc(*b"Pgst"), fourcc(*b"expo")),
        IworkApplication::Keynote => (fourcc(*b"Knst"), fourcc(*b"expo")),
    };
    send_event_three(
        document,
        event_class,
        event_id,
        EXPORT_DESTINATION_PARAMETER,
        url,
        EXPORT_FORMAT_PARAMETER,
        format,
        EXPORT_PROPERTIES_PARAMETER,
        std::ptr::null_mut(),
    );
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
    {
        Ok(())
    } else {
        Err(failure(
            AgentErrorKind::TransportError,
            "iWork did not create the requested batch validation export",
            true,
        ))
    }
}

unsafe fn close_without_saving(document: *mut AnyObject) -> Result<(), AgentError> {
    let saving = enum_descriptor(SAVE_NO)?;
    send_event_two(
        document,
        CORE_SUITE,
        CLOSE_EVENT,
        SAVE_OPTIONS_PARAMETER,
        saving,
        FILE_PARAMETER,
        std::ptr::null_mut(),
    );
    Ok(())
}

unsafe fn enum_descriptor(code: u32) -> Result<*mut AnyObject, AgentError> {
    let descriptor: *mut AnyObject =
        msg_send![class!(NSAppleEventDescriptor), descriptorWithEnumCode: code];
    if descriptor.is_null() {
        Err(failure(
            AgentErrorKind::Internal,
            "cannot construct an iWork Apple event enum",
            true,
        ))
    } else {
        Ok(descriptor)
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn send_event_two(
    receiver: *mut AnyObject,
    event_class: u32,
    event_id: u32,
    first_code: u32,
    first_value: *mut AnyObject,
    second_code: u32,
    second_value: *mut AnyObject,
) -> *mut AnyObject {
    objc_msg_send_variadic(
        receiver,
        sel!(sendEvent:id:parameters:),
        event_class,
        event_id,
        first_code,
        first_value,
        second_code,
        second_value,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn send_event_three(
    receiver: *mut AnyObject,
    event_class: u32,
    event_id: u32,
    first_code: u32,
    first_value: *mut AnyObject,
    second_code: u32,
    second_value: *mut AnyObject,
    third_code: u32,
    third_value: *mut AnyObject,
) -> *mut AnyObject {
    objc_msg_send_variadic(
        receiver,
        sel!(sendEvent:id:parameters:),
        event_class,
        event_id,
        first_code,
        first_value,
        second_code,
        second_value,
        third_code,
        third_value,
        0,
    )
}

const fn native_format(application: IworkApplication) -> u32 {
    match application {
        IworkApplication::Numbers => NUMBERS_NATIVE_FORMAT,
        IworkApplication::Pages => PAGES_NATIVE_FORMAT,
        IworkApplication::Keynote => KEYNOTE_NATIVE_FORMAT,
    }
}

const fn export_format(application: IworkApplication, format: IworkBatchExportFormat) -> u32 {
    match (application, format) {
        (IworkApplication::Numbers, IworkBatchExportFormat::Pdf) => NUMBERS_PDF_FORMAT,
        (IworkApplication::Pages, IworkBatchExportFormat::Pdf) => PAGES_PDF_FORMAT,
        (IworkApplication::Keynote, IworkBatchExportFormat::Pdf) => KEYNOTE_PDF_FORMAT,
        (IworkApplication::Numbers, IworkBatchExportFormat::MicrosoftOffice) => {
            NUMBERS_MICROSOFT_OFFICE_FORMAT
        }
        (IworkApplication::Pages, IworkBatchExportFormat::MicrosoftOffice) => {
            PAGES_MICROSOFT_OFFICE_FORMAT
        }
        (IworkApplication::Keynote, IworkBatchExportFormat::MicrosoftOffice) => {
            KEYNOTE_MICROSOFT_OFFICE_FORMAT
        }
    }
}

unsafe fn front_document(app: *mut AnyObject) -> Result<*mut AnyObject, AgentError> {
    let documents = element_array(app, DOCUMENTS)?;
    first_element(documents, "the iWork application has no open document")
}

unsafe fn first_element(
    elements: *mut AnyObject,
    unavailable_message: &str,
) -> Result<*mut AnyObject, AgentError> {
    if elements.is_null() {
        return Err(unavailable(unavailable_message));
    }
    let count: usize = msg_send![elements, count];
    if count == 0 {
        return Err(unavailable(unavailable_message));
    }
    let object: *mut AnyObject = msg_send![elements, objectAtIndex: 0usize];
    if object.is_null() {
        Err(unavailable(unavailable_message))
    } else {
        Ok(object)
    }
}

unsafe fn document_identity(document: *mut AnyObject) -> Result<String, AgentError> {
    let name = property_string(document, NAME)?;
    let file = optional_property_string(document, FILE)?.unwrap_or_else(|| "unsaved".into());
    Ok(sha256_pair(&name, &file))
}

unsafe fn verify_app_version(app: *mut AnyObject, spec: AppSpec) -> Result<(), AgentError> {
    let version = property_string(app, VERSION)?;
    if version == spec.version {
        Ok(())
    } else {
        Err(failure(
            AgentErrorKind::UnsupportedCapability,
            "the installed iWork version is outside the frozen adapter contract",
            false,
        ))
    }
}

fn verify_contract(spec: AppSpec) -> Result<(), AgentError> {
    let bytes = fs::read(spec.sdef_path).map_err(|_| {
        failure(
            AgentErrorKind::UnsupportedCapability,
            "the frozen iWork scripting dictionary is unavailable",
            false,
        )
    })?;
    if sha256(&bytes) == spec.sdef_sha256 {
        Ok(())
    } else {
        Err(failure(
            AgentErrorKind::UnsupportedCapability,
            "the iWork scripting dictionary does not match the frozen adapter contract",
            false,
        ))
    }
}

fn application_is_running(bundle_id: &str) -> Result<bool, AgentError> {
    let spec = APP_SPECS
        .iter()
        .copied()
        .find(|spec| spec.bundle_id == bundle_id)
        .ok_or_else(|| {
            failure(
                AgentErrorKind::Internal,
                "an unregistered iWork bundle was requested",
                false,
            )
        })?;
    Ok(running_process_id(spec).is_some())
}

fn running_process_id(spec: AppSpec) -> Option<i32> {
    let system = System::new_all();
    system.processes().iter().find_map(|(pid, process)| {
        process
            .exe()
            .is_some_and(|path| path == std::path::Path::new(spec.executable_path))
            .then_some(pid.as_u32() as i32)
    })
}

fn with_bridge<T>(
    operation: impl FnOnce() -> Result<T, AgentError> + UnwindSafe,
) -> Result<Result<T, AgentError>, AgentError> {
    autoreleasepool(|_| {
        // SAFETY: the closure never panics and contains only synchronous Objective-C sends.
        unsafe { objc2::exception::catch(operation) }.map_err(|_| {
            failure(
                AgentErrorKind::TransportError,
                "ScriptingBridge raised an exception",
                true,
            )
        })
    })
}

unsafe fn nsstring(value: &str) -> Result<*mut AnyObject, AgentError> {
    let value = CString::new(value).map_err(|_| {
        failure(
            AgentErrorKind::InvalidInput,
            "iWork text contains a NUL byte",
            false,
        )
    })?;
    let object: *mut AnyObject = msg_send![class!(NSString), stringWithUTF8String: value.as_ptr()];
    if object.is_null() {
        Err(failure(
            AgentErrorKind::Internal,
            "cannot allocate an iWork string",
            true,
        ))
    } else {
        Ok(object)
    }
}

unsafe fn property_object(object: *mut AnyObject, code: u32) -> Result<*mut AnyObject, AgentError> {
    let property: *mut AnyObject = msg_send![object, propertyWithCode: code];
    if property.is_null() {
        Err(failure(
            AgentErrorKind::TransportError,
            "iWork could not resolve a frozen semantic property",
            true,
        ))
    } else {
        Ok(property)
    }
}

unsafe fn element_array(object: *mut AnyObject, code: u32) -> Result<*mut AnyObject, AgentError> {
    let elements: *mut AnyObject = msg_send![object, elementArrayWithCode: code];
    if elements.is_null() {
        Err(failure(
            AgentErrorKind::TransportError,
            "iWork could not resolve a frozen semantic element collection",
            true,
        ))
    } else {
        Ok(elements)
    }
}

unsafe fn set_property_string(
    object: *mut AnyObject,
    code: u32,
    value: &str,
    _error_message: &str,
) -> Result<(), AgentError> {
    let property = property_object(object, code)?;
    let value = nsstring(value)?;
    let _: () = msg_send![property, setTo: value];
    Ok(())
}

unsafe fn property_string(object: *mut AnyObject, code: u32) -> Result<String, AgentError> {
    optional_property_string(object, code)?.ok_or_else(|| {
        failure(
            AgentErrorKind::TransportError,
            "iWork returned no value for a frozen semantic property",
            true,
        )
    })
}

unsafe fn optional_property_string(
    object: *mut AnyObject,
    code: u32,
) -> Result<Option<String>, AgentError> {
    let property = property_object(object, code)?;
    let value: *mut AnyObject = msg_send![property, get];
    if value.is_null() {
        return Ok(None);
    }
    let is_string: bool = msg_send![value, isKindOfClass: class!(NSString)];
    let concrete: *mut AnyObject = if is_string {
        value
    } else {
        msg_send![value, description]
    };
    if concrete.is_null() {
        return Ok(None);
    }
    let bytes: *const c_char = msg_send![concrete, UTF8String];
    if bytes.is_null() {
        return Err(failure(
            AgentErrorKind::TransportError,
            "iWork returned a non-textual semantic value",
            true,
        ));
    }
    Ok(Some(CStr::from_ptr(bytes).to_string_lossy().into_owned()))
}

fn ensure_automation_permission(spec: AppSpec) -> Result<(), AgentError> {
    match crate::macos_permissions::automation_permission(spec.bundle_id) {
        crate::macos_permissions::AutomationPermissionState::Granted => Ok(()),
        crate::macos_permissions::AutomationPermissionState::Missing => Err(failure(
            AgentErrorKind::PermissionDenied,
            "macOS Automation permission for the iWork application is missing",
            false,
        )),
        crate::macos_permissions::AutomationPermissionState::TargetOffline => Err(failure(
            AgentErrorKind::TargetOffline,
            "the iWork application is not running",
            true,
        )),
        crate::macos_permissions::AutomationPermissionState::Failed => Err(failure(
            AgentErrorKind::TransportError,
            "macOS could not determine iWork Automation permission",
            true,
        )),
    }
}

fn spec(application: IworkApplication) -> AppSpec {
    APP_SPECS
        .iter()
        .copied()
        .find(|spec| spec.app == application)
        .expect("every closed iWork application has a frozen spec")
}

const fn fourcc(bytes: [u8; 4]) -> u32 {
    u32::from_be_bytes(bytes)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_pair(left: &str, right: &str) -> String {
    let mut digest = Sha256::new();
    digest.update((left.len() as u64).to_be_bytes());
    digest.update(left.as_bytes());
    digest.update((right.len() as u64).to_be_bytes());
    digest.update(right.as_bytes());
    format!("{:x}", digest.finalize())
}

fn stale(message: &str) -> AgentError {
    failure(AgentErrorKind::InvalidInput, message, false)
}

fn unavailable(message: &str) -> AgentError {
    failure(AgentErrorKind::SessionUnavailable, message, true)
}

fn failure(kind: AgentErrorKind, message: &str, retryable: bool) -> AgentError {
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
    fn frozen_contracts_match_the_installed_iwork_dictionaries() {
        for spec in APP_SPECS {
            verify_contract(spec).unwrap();
        }
    }

    #[test]
    fn adapter_surface_contains_only_the_three_reviewed_applications() {
        assert_eq!(APP_SPECS.len(), 3);
        assert_eq!(
            spec(IworkApplication::Numbers).bundle_id,
            "com.apple.Numbers"
        );
        assert_eq!(spec(IworkApplication::Pages).bundle_id, "com.apple.Pages");
        assert_eq!(
            spec(IworkApplication::Keynote).bundle_id,
            "com.apple.Keynote"
        );
        assert!(IWORK_ADAPTER_VERSION.starts_with("iwork-scripting-bridge/"));
        assert_eq!(native_format(IworkApplication::Numbers), fourcc(*b"Nuff"));
        assert_eq!(native_format(IworkApplication::Pages), fourcc(*b"Pgff"));
        assert_eq!(native_format(IworkApplication::Keynote), fourcc(*b"Knff"));
        assert_eq!(
            export_format(
                IworkApplication::Numbers,
                IworkBatchExportFormat::MicrosoftOffice
            ),
            fourcc(*b"Nexl")
        );
        assert_eq!(
            export_format(IworkApplication::Pages, IworkBatchExportFormat::Pdf),
            fourcc(*b"Ppdf")
        );
        assert_eq!(
            export_format(IworkApplication::Keynote, IworkBatchExportFormat::Pdf),
            fourcc(*b"Kpdf")
        );
    }

    #[test]
    fn readiness_probe_does_not_launch_apps() {
        let reports = readiness();
        assert_eq!(reports.len(), 3);
        assert!(reports.iter().all(|report| report.contract_verified));
    }

    #[test]
    #[ignore = "requires a running iWork application, an open document, and Automation permission"]
    fn live_observation_uses_the_production_scripting_bridge_adapter() {
        let app = std::env::var("LRD_IWORK_LIVE_APP").expect("set LRD_IWORK_LIVE_APP");
        let app = match app.as_str() {
            "numbers" => IworkApplication::Numbers,
            "pages" => IworkApplication::Pages,
            "keynote" => IworkApplication::Keynote,
            _ => panic!("LRD_IWORK_LIVE_APP must be numbers, pages, or keynote"),
        };
        observe(app).unwrap();
    }

    #[test]
    #[ignore = "requires a running iWork app, Automation permission, and LRD_IWORK_BATCH_SOURCE"]
    fn live_batch_observation_opens_the_selected_file_and_closes_without_saving() {
        let app = std::env::var("LRD_IWORK_LIVE_APP").expect("set LRD_IWORK_LIVE_APP");
        let source = std::env::var("LRD_IWORK_BATCH_SOURCE")
            .expect("set LRD_IWORK_BATCH_SOURCE to a disposable native iWork file");
        let application = match app.as_str() {
            "numbers" => IworkApplication::Numbers,
            "pages" => IworkApplication::Pages,
            "keynote" => IworkApplication::Keynote,
            _ => panic!("LRD_IWORK_LIVE_APP must be numbers, pages, or keynote"),
        };
        let before = std::fs::read(&source).unwrap();
        observe_batch(application, Path::new(&source)).unwrap();
        assert_eq!(std::fs::read(source).unwrap(), before);
    }

    #[test]
    #[ignore = "requires a running iWork app, Automation permission, and a disposable LRD_IWORK_BATCH_SOURCE"]
    fn live_batch_mutation_saves_verified_native_and_pdf_copies_without_changing_source() {
        let app = std::env::var("LRD_IWORK_LIVE_APP").expect("set LRD_IWORK_LIVE_APP");
        let source = std::env::var("LRD_IWORK_BATCH_SOURCE")
            .expect("set LRD_IWORK_BATCH_SOURCE to a disposable native iWork file");
        let (application, extension) = match app.as_str() {
            "numbers" => (IworkApplication::Numbers, "numbers"),
            "pages" => (IworkApplication::Pages, "pages"),
            "keynote" => (IworkApplication::Keynote, "key"),
            _ => panic!("LRD_IWORK_LIVE_APP must be numbers, pages, or keynote"),
        };
        let marker = format!("LRD_IWORK_BATCH_{}", std::process::id());
        let output_parent = Path::new(&source).parent().unwrap();
        let native = output_parent.join(format!("{marker}.{extension}"));
        let pdf = output_parent.join(format!("{marker}.pdf"));
        let before = std::fs::read(&source).unwrap();
        let observed = observe_batch(application, Path::new(&source)).unwrap();
        let output = IworkBatchOutput {
            native_path: &native,
            export: Some((IworkBatchExportFormat::Pdf, &pdf)),
        };
        let changed = match observed {
            IworkObservation::Numbers { locator, .. } => apply_numbers_batch(
                Path::new(&source),
                &output,
                &locator,
                &SpreadsheetLivePatchAction::SetCellValue {
                    value: marker.clone(),
                },
            ),
            IworkObservation::Pages { locator, .. } => apply_pages_batch(
                Path::new(&source),
                &output,
                &locator,
                &DocumentLivePatchAction::ReplaceBodyText {
                    text: marker.clone(),
                },
            ),
            IworkObservation::Keynote { locator, .. } => apply_keynote_batch(
                Path::new(&source),
                &output,
                &locator,
                &PresentationLivePatchAction::ReplaceSlideTitle {
                    text: marker.clone(),
                },
            ),
        }
        .unwrap();
        assert!(changed.changed && changed.verified);
        assert_eq!(std::fs::read(&source).unwrap(), before);
        assert!(native.exists());
        assert!(pdf.metadata().unwrap().len() > 0);
        let copied = observe_batch(application, &native).unwrap();
        match copied {
            IworkObservation::Numbers { value, .. } => assert_eq!(value, marker),
            IworkObservation::Pages { body_text, .. } => assert_eq!(body_text, marker),
            IworkObservation::Keynote { title, .. } => assert_eq!(title, marker),
        }
        std::fs::remove_file(native).unwrap();
        std::fs::remove_file(pdf).unwrap();
    }

    #[test]
    #[ignore = "requires a running iWork application, an open document, and Automation permission"]
    fn live_stale_locator_is_rejected_before_mutation() {
        let app = std::env::var("LRD_IWORK_LIVE_APP").expect("set LRD_IWORK_LIVE_APP");
        let (application, before, error) = match app.as_str() {
            "numbers" => {
                let before = observe(IworkApplication::Numbers).unwrap();
                let IworkObservation::Numbers { mut locator, .. } = before.clone() else {
                    unreachable!();
                };
                locator.before_sha256 = "stale".into();
                (
                    IworkApplication::Numbers,
                    before,
                    apply_numbers(
                        &locator,
                        &SpreadsheetLivePatchAction::SetCellValue {
                            value: "must-not-be-written".into(),
                        },
                    )
                    .unwrap_err(),
                )
            }
            "pages" => {
                let before = observe(IworkApplication::Pages).unwrap();
                let IworkObservation::Pages { mut locator, .. } = before.clone() else {
                    unreachable!();
                };
                locator.before_sha256 = "stale".into();
                (
                    IworkApplication::Pages,
                    before,
                    apply_pages(
                        &locator,
                        &DocumentLivePatchAction::ReplaceBodyText {
                            text: "must-not-be-written".into(),
                        },
                    )
                    .unwrap_err(),
                )
            }
            "keynote" => {
                let before = observe(IworkApplication::Keynote).unwrap();
                let IworkObservation::Keynote { mut locator, .. } = before.clone() else {
                    unreachable!();
                };
                locator.title_before_sha256 = "stale".into();
                (
                    IworkApplication::Keynote,
                    before,
                    apply_keynote(
                        &locator,
                        &PresentationLivePatchAction::ReplaceSlideTitle {
                            text: "must-not-be-written".into(),
                        },
                    )
                    .unwrap_err(),
                )
            }
            _ => panic!("LRD_IWORK_LIVE_APP must be numbers, pages, or keynote"),
        };
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);
        assert!(!error.retryable);
        assert_eq!(observe(application).unwrap(), before);
    }

    #[test]
    #[ignore = "mutates the front iWork document; use only with a disposable test document"]
    fn live_mutation_round_trip_uses_typed_actions_and_exact_readback() {
        let app = std::env::var("LRD_IWORK_LIVE_APP").expect("set LRD_IWORK_LIVE_APP");
        let marker = format!("LRD_IWORK_TYPED_ROUND_TRIP_{}", std::process::id());
        match app.as_str() {
            "numbers" => {
                let IworkObservation::Numbers {
                    locator,
                    value,
                    formula,
                    ..
                } = observe(IworkApplication::Numbers).unwrap()
                else {
                    unreachable!();
                };
                let changed = apply_numbers(
                    &locator,
                    &SpreadsheetLivePatchAction::SetCellValue {
                        value: marker.clone(),
                    },
                )
                .unwrap();
                assert!(changed.changed && changed.verified);
                let IworkObservation::Numbers {
                    locator: restore_locator,
                    ..
                } = observe(IworkApplication::Numbers).unwrap()
                else {
                    unreachable!();
                };
                let restore = formula.map_or(
                    SpreadsheetLivePatchAction::SetCellValue { value },
                    |formula| SpreadsheetLivePatchAction::SetCellFormula { formula },
                );
                assert!(apply_numbers(&restore_locator, &restore).unwrap().verified);
            }
            "pages" => {
                let IworkObservation::Pages { locator, body_text } =
                    observe(IworkApplication::Pages).unwrap()
                else {
                    unreachable!();
                };
                let changed = apply_pages(
                    &locator,
                    &DocumentLivePatchAction::ReplaceBodyText {
                        text: marker.clone(),
                    },
                )
                .unwrap();
                assert!(changed.changed && changed.verified);
                let IworkObservation::Pages {
                    locator: restore_locator,
                    ..
                } = observe(IworkApplication::Pages).unwrap()
                else {
                    unreachable!();
                };
                assert!(
                    apply_pages(
                        &restore_locator,
                        &DocumentLivePatchAction::ReplaceBodyText { text: body_text }
                    )
                    .unwrap()
                    .verified
                );
            }
            "keynote" => {
                let IworkObservation::Keynote { locator, title, .. } =
                    observe(IworkApplication::Keynote).unwrap()
                else {
                    unreachable!();
                };
                let changed = apply_keynote(
                    &locator,
                    &PresentationLivePatchAction::ReplaceSlideTitle { text: marker },
                )
                .unwrap();
                assert!(changed.changed && changed.verified);
                let IworkObservation::Keynote {
                    locator: restore_locator,
                    ..
                } = observe(IworkApplication::Keynote).unwrap()
                else {
                    unreachable!();
                };
                assert!(
                    apply_keynote(
                        &restore_locator,
                        &PresentationLivePatchAction::ReplaceSlideTitle { text: title }
                    )
                    .unwrap()
                    .verified
                );
            }
            _ => panic!("LRD_IWORK_LIVE_APP must be numbers, pages, or keynote"),
        }
    }

    #[test]
    #[ignore = "mutates the front Numbers document; use only with a disposable test document"]
    fn live_numbers_formula_reports_host_normalization_and_restores_the_cell() {
        let IworkObservation::Numbers {
            locator,
            value,
            formula,
            ..
        } = observe(IworkApplication::Numbers).unwrap()
        else {
            unreachable!();
        };
        let applied = apply_numbers(
            &locator,
            &SpreadsheetLivePatchAction::SetCellFormula {
                formula: "=1+41".into(),
            },
        )
        .unwrap();
        assert!(applied.changed);
        let IworkObservation::Numbers {
            locator: restore_locator,
            formatted_value,
            ..
        } = observe(IworkApplication::Numbers).unwrap()
        else {
            unreachable!();
        };
        assert_eq!(formatted_value, "42");
        let restore = formula.map_or(
            SpreadsheetLivePatchAction::SetCellValue { value },
            |formula| SpreadsheetLivePatchAction::SetCellFormula { formula },
        );
        let restored = apply_numbers(&restore_locator, &restore).unwrap();
        assert!(restored.verified || restored.changed);
    }
}
