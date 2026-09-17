//! Windows XLSX batch tools. No active selection, add-in or Live pairing.
use super::*;
pub const PROVIDER_ID: &str = "office.xlsx.batch";
pub const INSPECT_CAPABILITY_ID: &str = "office.xlsx.batch.inspect";
pub const PATCH_CAPABILITY_ID: &str = "office.xlsx.batch.patch";
pub const ADAPTER_ID: &str = "windows.office.xlsx.batch";
pub const INSPECT_TOOL: &str = "inspect_excel_cell";
pub const PATCH_TOOL: &str = "patch_excel_copy";
pub(super) const READINESS_IDENTITIES: [(&str, &str, &str, &str); 1] = [(
    PROVIDER_ID,
    INSPECT_CAPABILITY_ID,
    ADAPTER_ID,
    desk_agent_protocol::computer_use::office_batch::XLSX_ADAPTER_VERSION,
)];
pub(super) const PATCH_READINESS_IDENTITIES: [(&str, &str, &str, &str); 1] = [(
    PROVIDER_ID,
    PATCH_CAPABILITY_ID,
    ADAPTER_ID,
    desk_agent_protocol::computer_use::office_batch::XLSX_ADAPTER_VERSION,
)];

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectArgs {
    pub sheet_name: String,
    pub address: String,
    pub max_bytes: u32,
}
pub(super) fn inspect_tool() -> RegisteredTool {
    let mut tool = batch_inspect_tool(
        INSPECT_TOOL,
        "Read one explicitly named worksheet and canonical A1 cell from exactly one owner-selected XLSX file. Use inspect_spreadsheets first if the worksheet name is unknown. No native path, source token, active document or Live session is accepted. Value is JSON stored evidence (presence, type, literal text/value); formula caches are omitted and formulas are not calculated. Use the returned fresh range for a copy mutation.",
        Capability::SpreadsheetLiveInspect,
    );
    tool.spec.parameters_schema = serde_json::json!({
        "type":"object", "additionalProperties":false,
        "properties": {
            "sheet_name":{"type":"string","minLength":1,"maxLength":31},
            "address":{"type":"string","pattern":"^[A-Z]{1,3}[1-9][0-9]{0,6}$"},
            "max_bytes":{"type":"integer","minimum":1024,"maximum":65536}
        },
        "required":["sheet_name","address","max_bytes"]
    });
    tool
}
pub(super) fn patch_tool() -> RegisteredTool {
    let mut tool = spreadsheet_batch_patch_tool();
    tool.spec.name = PATCH_TOOL.into();
    tool.spec.description = "Update exactly one fresh range returned by inspect_excel_cell and create a new XLSX in the owner-approved output directory. set_cell_value writes literal text without numeric, date or formula coercion; set_cell_formula uses the restricted formula policy. Read first, then request exact mutation approval with the entire target/output/action. Never overwrite or operate on the user's active workbook. Formula results require current-run calculation and independent file readback before publication.".into();
    tool.spec.parameters_schema["properties"]["output"] = batch_output_schema("xlsx");
    let actions = tool.spec.parameters_schema["properties"]["action"]["oneOf"]
        .as_array_mut()
        .expect("shared spreadsheet action schema");
    for (kind, value_schema) in [
        (
            "set_cell_number",
            serde_json::json!({"type":"string","minLength":1,"maxLength":128,"description":"Finite decimal or exponent number, without surrounding whitespace"}),
        ),
        ("set_cell_boolean", serde_json::json!({"type":"boolean"})),
    ] {
        actions.push(serde_json::json!({
            "type":"object","additionalProperties":false,"required":["kind","params"],
            "properties":{"kind":{"type":"string","const":kind},"params":{
                "type":"object","additionalProperties":false,"required":["value"],"properties":{"value":value_schema}
            }}
        }));
    }
    tool.spec.description.push_str(" For an actual numeric or boolean cell use set_cell_number (finite decimal string) or set_cell_boolean (JSON boolean); do not encode those as set_cell_value text.");
    tool
}
pub(super) fn provider() -> ProviderDescriptor {
    let read = provider_for_tool(
        PROVIDER_ID,
        INSPECT_CAPABILITY_ID,
        "assistant.capability.excelBatchInspect",
        vec![ADAPTER_ID.into()],
        ExecutionLocality::Edge,
        CapabilityEffect::ReadFile,
        1,
        Vec::new(),
        vec![CapabilityDataCategory::FileContent],
        vec![AuthorizationResourceKind::FreshObjectReference],
        inspect_tool(),
    );
    let write = provider_for_tool(
        PROVIDER_ID,
        PATCH_CAPABILITY_ID,
        "assistant.capability.excelBatchPatch",
        vec![ADAPTER_ID.into()],
        ExecutionLocality::Edge,
        CapabilityEffect::MutateApplication,
        1,
        Vec::new(),
        vec![CapabilityDataCategory::FileContent],
        vec![AuthorizationResourceKind::FreshObjectReference],
        patch_tool(),
    );
    let mut descriptor = merge_provider_capabilities(read, write);
    for capability in &mut descriptor.wire.capabilities {
        capability.prerequisites.platforms = vec![CapabilityPlatform::Windows];
    }
    for capability in &mut descriptor.capabilities {
        capability.wire.prerequisites.platforms = vec![CapabilityPlatform::Windows];
    }
    descriptor
}
pub(super) fn adapter(providers: &ProviderRegistry) -> EdgeAdapterDescriptor {
    EdgeAdapterDescriptor {
        adapter_id: ADAPTER_ID.into(),
        adapter_version: desk_agent_protocol::computer_use::office_batch::XLSX_ADAPTER_VERSION
            .into(),
        capability_ids: vec![INSPECT_CAPABILITY_ID.into(), PATCH_CAPABILITY_ID.into()],
        limits: providers
            .capability(INSPECT_CAPABILITY_ID)
            .expect("static XLSX read exists")
            .wire
            .limits,
    }
}
