//! Closed error domains shared by aggregation, queries and detail filtering.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{
    InputIssue, OutputOutcome, RequestOutcome,
    aggregate::{Rate, known_request_error},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ErrorSelector {
    Request(RequestOutcome),
    Output(OutputOutcome),
    Input(InputIssue),
}

impl ErrorSelector {
    pub fn supports(self, rate: Rate) -> bool {
        match (self, rate) {
            (Self::Request(_), Rate::RequestFailure | Rate::AttemptFailure)
            | (Self::Output(_), Rate::OutputRejection) => true,
            (Self::Input(issue), Rate::InputRejection) => issue != InputIssue::SchemaUnavailable,
            (Self::Input(issue), Rate::ToolFormatFailure) => matches!(
                issue,
                InputIssue::UnknownTool
                    | InputIssue::UnexposedTool
                    | InputIssue::InvalidProtocol
                    | InputIssue::InvalidJson
                    | InputIssue::MissingField
                    | InputIssue::UnknownField
                    | InputIssue::Type
                    | InputIssue::Enum
                    | InputIssue::Pattern
                    | InputIssue::Length
                    | InputIssue::Combination
            ),
            (Self::Input(issue), Rate::ReferenceFailure) => matches!(
                issue,
                InputIssue::UnknownReference
                    | InputIssue::ReferenceType
                    | InputIssue::ReferenceExpired
                    | InputIssue::ReferenceUnavailable
            ),
            _ => false,
        }
    }

    pub fn identifier(self) -> String {
        fn name(value: impl Serialize) -> String {
            serde_json::to_value(value)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".into())
        }
        match self {
            Self::Request(value) => format!("request.{}", name(value)),
            Self::Output(value) => format!("output.{}", name(value)),
            Self::Input(value) => format!("input.{}", name(value)),
        }
    }

    pub fn category(self) -> String {
        self.identifier()
            .split_once('.')
            .map_or_else(String::new, |(_, value)| value.to_owned())
    }
}

impl FromStr for ErrorSelector {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        fn parse<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, &'static str> {
            serde_json::from_value(serde_json::Value::String(value.to_owned()))
                .map_err(|_| "unsupported error category")
        }
        match value.split_once('.') {
            Some(("request", value)) => {
                let value = parse::<RequestOutcome>(value)?;
                if known_request_error(value) {
                    Ok(Self::Request(value))
                } else {
                    Err("unsupported request error")
                }
            }
            Some(("output", value)) => {
                let value = parse::<OutputOutcome>(value)?;
                if matches!(
                    value,
                    OutputOutcome::InvalidProtocol
                        | OutputOutcome::InvalidStructuredOutput
                        | OutputOutcome::EmptyResponse
                ) {
                    Ok(Self::Output(value))
                } else {
                    Err("unsupported output rejection")
                }
            }
            Some(("input", value)) => {
                let value = parse::<InputIssue>(value)?;
                if value != InputIssue::None {
                    Ok(Self::Input(value))
                } else {
                    Err("unsupported input issue")
                }
            }
            _ => Err("error category requires request, output or input domain"),
        }
    }
}

impl Serialize for ErrorSelector {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.identifier())
    }
}

impl<'de> Deserialize<'de> for ErrorSelector {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ErrorSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.identifier())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn domains_are_closed_and_similarly_named_errors_stay_distinct() {
        let output: ErrorSelector = "output.invalid_protocol".parse().unwrap();
        let input: ErrorSelector = "input.invalid_protocol".parse().unwrap();
        assert_ne!(output, input);
        assert!(output.supports(Rate::OutputRejection));
        assert!(!output.supports(Rate::ToolFormatFailure));
        assert!(input.supports(Rate::ToolFormatFailure));
        assert!(!input.supports(Rate::RequestFailure));
        for value in [
            "invalid_protocol",
            "request.returned",
            "input.none",
            "output.policy_rejected",
            "request.arbitrary",
        ] {
            assert!(value.parse::<ErrorSelector>().is_err(), "{value}");
        }
        assert!(
            !"input.schema_unavailable"
                .parse::<ErrorSelector>()
                .unwrap()
                .supports(Rate::InputRejection)
        );
    }

    #[test]
    fn typed_selectors_round_trip_as_json_map_keys() {
        let input: ErrorSelector = "input.reference_expired".parse().unwrap();
        let map = std::collections::BTreeMap::from([(input, 17u64)]);
        let json = serde_json::to_string(&map).unwrap();
        assert_eq!(json, "{\"input.reference_expired\":17}");
        assert_eq!(
            serde_json::from_str::<std::collections::BTreeMap<ErrorSelector, u64>>(&json).unwrap(),
            map
        );
    }
}
