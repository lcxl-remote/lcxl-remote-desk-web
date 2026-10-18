//! Inspect envelope fields without building a second owned JSON tree.
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct EnvelopeProbe {
    pub version: u64,
    pub has_disclosure: bool,
    pub has_focus_epoch: bool,
    pub has_visual_evidence: bool,
    pub has_visual_verification: bool,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum Field {
    ConversationSchemaVersion,
    CapabilityDisclosure,
    FocusEpoch,
    VisualEvidence,
    PendingVisualVerification,
    #[serde(other)]
    Other,
}

// deserialize_any traverses every compound value through serde_json's existing
// recursion guard. deserialize_ignored_any would bypass that depth boundary.
struct ProbeValue(Option<u64>);
impl<'de> Deserialize<'de> for ProbeValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;
        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = ProbeValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<Self::Value, E> {
                Ok(ProbeValue(Some(n)))
            }
            fn visit_i64<E: serde::de::Error>(self, n: i64) -> Result<Self::Value, E> {
                Ok(ProbeValue(u64::try_from(n).ok()))
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(ProbeValue(None))
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(ProbeValue(None))
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(ProbeValue(None))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(ProbeValue(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<ProbeValue>()?.is_some() {}
                Ok(ProbeValue(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, ProbeValue>()?.is_some() {}
                Ok(ProbeValue(None))
            }
        }
        deserializer.deserialize_any(ValueVisitor)
    }
}

impl<'de> Deserialize<'de> for EnvelopeProbe {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EnvelopeVisitor;
        impl<'de> Visitor<'de> for EnvelopeVisitor {
            type Value = EnvelopeProbe;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a session envelope")
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(EnvelopeProbe::default())
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(EnvelopeProbe::default())
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(EnvelopeProbe::default())
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(EnvelopeProbe::default())
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(EnvelopeProbe::default())
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(EnvelopeProbe::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<ProbeValue>()?.is_some() {}
                Ok(EnvelopeProbe::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut result = EnvelopeProbe::default();
                while let Some(field) = map.next_key::<Field>()? {
                    let value = map.next_value::<ProbeValue>()?;
                    match field {
                        Field::ConversationSchemaVersion => result.version = value.0.unwrap_or(0),
                        Field::CapabilityDisclosure => result.has_disclosure = true,
                        Field::FocusEpoch => result.has_focus_epoch = true,
                        Field::VisualEvidence => result.has_visual_evidence = true,
                        Field::PendingVisualVerification => result.has_visual_verification = true,
                        Field::Other => {}
                    }
                }
                Ok(result)
            }
        }
        deserializer.deserialize_any(EnvelopeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn equivalent(input: &str) {
        let before = serde_json::from_str::<serde_json::Value>(input).map(|value| EnvelopeProbe {
            version: value
                .get("conversation_schema_version")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            has_disclosure: value.get("capability_disclosure").is_some(),
            has_focus_epoch: value.get("focus_epoch").is_some(),
            has_visual_evidence: value.get("visual_evidence").is_some(),
            has_visual_verification: value.get("pending_visual_verification").is_some(),
        });
        let after = serde_json::from_str::<EnvelopeProbe>(input);
        assert_eq!(before.is_ok(), after.is_ok(), "acceptance changed");
        if let (Ok(before), Ok(after)) = (before, after) {
            assert_eq!(before, after);
        }
    }
    #[test]
    fn preserves_presence_null_duplicate_and_version_values() {
        for value in [
            "null",
            "0",
            "1",
            "99999999999999999999",
            "-1",
            "1.0",
            "true",
            "\"future\"",
            "[]",
            "{}",
        ] {
            equivalent(&format!(
                r#"{{"conversation_schema_version":{value},"pending_visual_verification":null,"capability_disclosure":[],"visual_evidence":{{}},"focus_epoch":0}}"#
            ));
        }
        for input in [
            "{}",
            "[]",
            "null",
            "true",
            "0",
            "-1",
            "1.5",
            "\"scalar\"",
            "[{},[null]]",
        ] {
            equivalent(input);
        }
        equivalent(
            r#"{"conversation_schema_version":1,"conversation_schema_version":2,"pending_visual_verification":null}"#,
        );
        equivalent(r#"{"focus\u005fepoch":null,"unknown":{"a":[null,false,"escaped\\value"]}}"#);
    }
    #[test]
    fn preserves_recursion_limit_for_unknown_fields() {
        for depth in [1, 8, 64, 125, 126, 127, 128, 129, 512] {
            equivalent(&format!(
                r#"{{"unknown":{}0{}}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            ));
            equivalent(&format!(
                r#"{{"unknown":{}0{}}}"#,
                r#"{"a":"#.repeat(depth),
                "}".repeat(depth)
            ));
        }
    }
    #[test]
    fn rejects_malformed_unknown_values() {
        for input in [
            "{",
            r#"{"unknown":[1,]}"#,
            r#"{"unknown":"\uD800"}"#,
            r#"{"unknown":1e999}"#,
            r#"{"unknown":null} trailing"#,
        ] {
            equivalent(input);
        }
    }
}
