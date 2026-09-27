//! Runs the real Typst sandbox child (the built server executable) and proves
//! that hostile Typst is contained by the process boundary and its limits.
use std::path::Path;
use std::time::{Duration, Instant};

use desk_document_conversion::{
    ConversionKind, SourceFormat,
    sandbox::{SandboxRequest, SandboxResponse},
};
use lcxl_remote_desk_server::typst_sandbox::{self, SandboxFailure, SandboxLimits};

fn server() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_lcxl-remote-desk-server"))
}

fn convert(
    source: &str,
    limits: SandboxLimits,
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    typst_sandbox::run_with(
        server(),
        &[typst_sandbox::SUBCOMMAND],
        &SandboxRequest::Convert {
            kind: ConversionKind::TypstToPdf,
        },
        source.as_bytes(),
        limits,
    )
}

fn tight() -> SandboxLimits {
    SandboxLimits {
        memory_bytes: 512 * 1024 * 1024,
        cpu_seconds: 5,
        wall_clock: Duration::from_secs(10),
    }
}

#[test]
fn ordinary_typst_converts_and_renders_in_the_child() {
    let (response, pdf) = convert("= Report\nHello", SandboxLimits::default()).unwrap();
    assert!(matches!(response, SandboxResponse::Converted { .. }));
    assert!(pdf.starts_with(b"%PDF-"));
    let (response, png) = typst_sandbox::run_with(
        server(),
        &[typst_sandbox::SUBCOMMAND],
        &SandboxRequest::RenderPage {
            format: SourceFormat::Markdown,
            page: 1,
        },
        b"# Preview",
        SandboxLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        response,
        SandboxResponse::RenderedPage { page_count: 1, .. }
    ));
    assert!(png.starts_with(b"\x89PNG"));
}

#[cfg(target_os = "linux")]
#[test]
fn a_small_source_that_allocates_gigabytes_is_contained() {
    // 35 bytes of Typst that asks for about 1 GB: under the memory limit the
    // allocation fails and aborts only the child.
    let bomb = "#let large = \"x\" * 1000000000\nHello";
    assert!(bomb.len() <= 40);
    assert_eq!(convert(bomb, tight()), Err(SandboxFailure::ResourceLimit));
    // The worker and the next conversion are unaffected.
    assert!(convert("Hello", tight()).is_ok());
}

#[test]
fn a_non_terminating_source_is_stopped() {
    let started = Instant::now();
    let limits = SandboxLimits {
        memory_bytes: 512 * 1024 * 1024,
        cpu_seconds: 2,
        wall_clock: Duration::from_secs(3),
    };
    // Bounded but astronomically long: 10^10 loop iterations.
    let result = convert(
        "#let n = 0\n#for i in range(100000) { for j in range(100000) { n += 1 } }",
        limits,
    );
    assert!(
        matches!(
            result,
            Err(SandboxFailure::Timeout | SandboxFailure::ResourceLimit)
        ),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(convert("Hello", tight()).is_ok());
}

#[test]
fn compile_errors_are_structured_not_crashes() {
    let (response, payload) = convert("#panic(\"界\" * 1000)", tight()).unwrap();
    assert!(payload.is_empty());
    assert!(matches!(response, SandboxResponse::Failed { .. }));
}

#[test]
fn a_missing_sandbox_program_is_reported_as_unavailable() {
    let result = typst_sandbox::run_with(
        Path::new("/nonexistent/lcxl-typst-sandbox"),
        &[],
        &SandboxRequest::Convert {
            kind: ConversionKind::TextToPdf,
        },
        b"Hello",
        tight(),
    );
    assert!(matches!(result, Err(SandboxFailure::Unavailable(_))));
}
