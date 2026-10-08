//! Named scroll directions at the model boundary; native units remain unchanged.
use desk_agent_protocol::{AgentError, AgentErrorKind};
use serde_json::{Value, json};

fn invalid() -> AgentError {
    AgentError { kind: AgentErrorKind::InvalidInput, message: "Invalid scroll: use direction=up/down/left/right and positive amount (semantic: 1 or 2 increments; background: 1..10000 pixels). Do not mix direction with numeric axes. No action was executed.".into(), retryable:false, safe_for_model:true, error_code:None }
}

pub(crate) fn resolve(tool: &str, value: &mut Value) -> Result<(), AgentError> {
    if !matches!(tool, "execute_ui_actions" | "send_background_input") {
        return Ok(());
    }
    visit(value, tool == "send_background_input", true)
}

fn visit(value: &mut Value, background: bool, restore: bool) -> Result<(), AgentError> {
    if value["kind"] == "scroll" {
        let body = if background {
            value
        } else {
            &mut value["params"]
        };
        let (h, v) = if background {
            ("horizontal_pixels", "vertical_pixels")
        } else {
            ("horizontal", "vertical")
        };
        let object = body.as_object_mut().ok_or_else(invalid)?;
        if restore && object.contains_key("direction") {
            if object.contains_key(h) || object.contains_key(v) {
                return Err(invalid());
            }
            let amount = object
                .get("amount")
                .and_then(Value::as_i64)
                .filter(|amount| (1..=if background { 10000 } else { 2 }).contains(amount))
                .ok_or_else(invalid)?;
            let (horizontal, vertical) = match object.get("direction").and_then(Value::as_str) {
                Some("left") if !background => (-amount, 0),
                Some("right") if !background => (amount, 0),
                Some("up") => (0, -amount),
                Some("down") => (0, amount),
                _ => return Err(invalid()),
            };
            object.remove("direction");
            object.remove("amount");
            // Background mouse wheel deltas use the opposite vertical sign.
            object.insert(h.into(), json!(horizontal));
            object.insert(
                v.into(),
                json!(if background { -vertical } else { vertical }),
            );
        } else if !restore
            && let (Some(horizontal), Some(vertical)) = (
                object.get(h).and_then(Value::as_i64),
                object.get(v).and_then(Value::as_i64),
            )
        {
            if horizontal.unsigned_abs() > 10000 || vertical.unsigned_abs() > 10000 {
                return Ok(());
            }
            let vertical = if background { -vertical } else { vertical };
            let direction = match (horizontal, vertical) {
                (h, 0) if h < 0 && !background => Some(("left", -h)),
                (h, 0) if h > 0 && !background => Some(("right", h)),
                (0, v) if v < 0 => Some(("up", -v)),
                (0, v) if v > 0 => Some(("down", v)),
                _ => None,
            };
            if let Some((direction, amount)) = direction {
                object.remove(h);
                object.remove(v);
                object.insert("direction".into(), json!(direction));
                object.insert("amount".into(), json!(amount));
            }
        }
        return Ok(());
    }
    match value {
        Value::Object(object) => {
            for child in object.values_mut() {
                visit(child, background, restore)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                visit(child, background, restore)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn project_arguments(tool: &str, value: &mut Value) {
    if matches!(tool, "execute_ui_actions" | "send_background_input") {
        let _ = visit(value, tool == "send_background_input", false);
    }
}

pub(crate) fn project_tool(tool: &mut crate::chat::ToolSpec) {
    if !matches!(
        tool.name.as_str(),
        "execute_ui_actions" | "send_background_input"
    ) {
        return;
    }
    fn project(schema: &mut Value, background: bool) {
        if schema.pointer("/properties/kind/const") == Some(&json!("scroll")) {
            let body = if background {
                schema
            } else {
                &mut schema["properties"]["params"]
            };
            if body.pointer("/properties/direction").is_some() {
                return;
            }
            let (h, v) = if background {
                ("horizontal_pixels", "vertical_pixels")
            } else {
                ("horizontal", "vertical")
            };
            body["properties"]["direction"] = json!({"type":"string","enum":if background {json!(["up","down"])} else {json!(["up","down","left","right"])}});
            body["properties"]["amount"] = json!({"type":"integer","minimum":1,"maximum":if background {10000} else {2},"description":if background {"Distance in original window pixels; e.g. direction=down, amount=300."} else {"1=small increment, 2=large increment; e.g. direction=down, amount=1."}});
            if let Some(required) = body["required"].as_array_mut() {
                required.retain(|key| key != h && key != v);
            }
            body["oneOf"] = json!([
                {"required":["direction","amount"],"not":{"anyOf":[{"required":[h]},{"required":[v]}]}},
                {"required":[h,v],"not":{"anyOf":[{"required":["direction"]},{"required":["amount"]}]}}
            ]);
            return;
        }
        match schema {
            Value::Object(object) => {
                for child in object.values_mut() {
                    project(child, background);
                }
            }
            Value::Array(array) => {
                for child in array {
                    project(child, background);
                }
            }
            _ => {}
        }
    }
    project(
        &mut tool.parameters_schema,
        tool.name == "send_background_input",
    );
    tool.description.push_str(" Prefer semantic scroll direction=up/down/left/right or background direction=up/down with positive amount. Semantic amount is 1 (small) or 2 (large) increments; background amount is window pixels and still requires position. Numeric axes remain available for diagonal movement; never combine them with direction/amount.");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directions_roundtrip_without_changing_native_units() {
        for tool in ["execute_ui_actions", "send_background_input"] {
            for direction in ["up", "down", "left", "right"] {
                let background = tool == "send_background_input";
                if background && matches!(direction, "left" | "right") {
                    continue;
                }
                let params = json!({"direction":direction,"amount":if background {300} else {2}});
                let mut value = if background {
                    params.clone()
                } else {
                    json!({"params":params})
                };
                value["kind"] = json!("scroll");
                let original = value.clone();
                resolve(tool, &mut value).unwrap();
                if direction == "down" {
                    assert_eq!(
                        if background {
                            &value["vertical_pixels"]
                        } else {
                            &value["params"]["vertical"]
                        },
                        &json!(if background { -300 } else { 2 })
                    );
                }
                project_arguments(tool, &mut value);
                assert_eq!(value, original);
            }
        }
        let mut mixed =
            json!({"kind":"scroll","direction":"down","amount":300,"vertical_pixels":-300});
        assert!(resolve("send_background_input", &mut mixed).is_err());
    }
}
