//! Negotiate a SPA frame header without changing the selected video format.
use pipewire::{spa, stream::StreamRef};
use spa::pod::{Object, Pod, Property, Value, serialize::PodSerializer};

fn header_parameter() -> Object {
    Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(spa::utils::Id(spa::sys::SPA_META_Header)),
            ),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
            ),
        ],
    }
}

pub(super) fn request_header(stream: &StreamRef) -> Result<(), String> {
    let bytes = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(header_parameter()),
    )
    .map_err(|error| format!("serialize PipeWire header request: {error:?}"))?
    .0
    .into_inner();
    let pod = Pod::from_bytes(&bytes)
        .ok_or_else(|| "invalid PipeWire header request encoding".to_string())?;
    stream
        .update_params(&mut [pod])
        .map_err(|error| format!("request PipeWire header: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requests_only_header_type_and_native_header_size() {
        let parameter = header_parameter();
        assert_eq!(parameter.type_, spa::sys::SPA_TYPE_OBJECT_ParamMeta);
        assert_eq!(parameter.id, spa::sys::SPA_PARAM_Meta);
        assert_eq!(parameter.properties.len(), 2);
        assert_eq!(parameter.properties[0].key, spa::sys::SPA_PARAM_META_type);
        assert!(
            matches!(parameter.properties[0].value, Value::Id(spa::utils::Id(value)) if value == spa::sys::SPA_META_Header)
        );
        assert_eq!(parameter.properties[1].key, spa::sys::SPA_PARAM_META_size);
        assert!(
            matches!(parameter.properties[1].value, Value::Int(value) if value as usize == std::mem::size_of::<spa::sys::spa_meta_header>())
        );
    }
}
