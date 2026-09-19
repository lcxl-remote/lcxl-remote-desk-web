use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use desk_agent_protocol::service_status::MAX_SERVICE_PAGE_BYTES;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

#[derive(Serialize, Deserialize)]
struct Cursor {
    query: String,
    candidates: String,
    offset: usize,
}
fn key() -> &'static [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(rand::random)
}
fn encode(cursor: &Cursor) -> String {
    let bytes = serde_json::to_vec(cursor).expect("cursor serializes");
    let mut mac = Hmac::<Sha256>::new_from_slice(key()).expect("valid key");
    mac.update(&bytes);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&bytes),
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}
fn decode(raw: &str) -> Result<Cursor, AgentError> {
    let bad =
        || invalid("Service cursor is invalid or belongs to another worker; restart the query");
    let (payload, signature) = raw.split_once('.').ok_or_else(bad)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).map_err(|_| bad())?;
    let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| bad())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key()).expect("valid key");
    mac.update(&bytes);
    mac.verify_slice(&signature).map_err(|_| bad())?;
    serde_json::from_slice(&bytes).map_err(|_| bad())
}
fn hash(value: &impl Serialize) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(
        serde_json::to_vec(value).expect("query serializes"),
    ))
}
fn matches_type(s: &ServiceEntry, p: &ServiceStatusParams) -> Option<bool> {
    if !p.start_types.is_empty() {
        return s.start_type.as_ref().map(|v| p.start_types.contains(v));
    }
    if !p.unit_file_states.is_empty() {
        return s
            .unit_file_state
            .as_ref()
            .map(|v| p.unit_file_states.contains(v));
    }
    if !p.launch_policies.is_empty() {
        return s
            .launch_policies
            .as_ref()
            .map(|vs| vs.iter().any(|v| p.launch_policies.contains(v)));
    }
    Some(true)
}

#[cfg(test)]
fn page(
    found: Enumeration,
    params: &ServiceStatusParams,
) -> Result<ServiceStatusOutput, AgentError> {
    page_with(found, params, |_| Ok(()))
}

pub(super) fn page_with(
    mut found: Enumeration,
    params: &ServiceStatusParams,
    mut enrich: impl FnMut(&mut ServiceEntry) -> Result<(), NativeDiagnostic>,
) -> Result<ServiceStatusOutput, AgentError> {
    found
        .services
        .sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));
    found
        .services
        .dedup_by(|a, b| a.scope == b.scope && a.name == b.name);
    let mut normalized = params.clone();
    normalized.cursor = None;
    normalized.limit = 50;
    for values in [
        &mut normalized.queries,
        &mut normalized.start_types,
        &mut normalized.unit_file_states,
        &mut normalized.launch_policies,
    ] {
        values.sort();
        values.dedup();
    }
    let query = hash(&(normalized, &found.scope));
    let candidates = hash(
        &found
            .services
            .iter()
            .map(|s| (&s.scope, &s.name))
            .collect::<Vec<_>>(),
    );
    let start = if let Some(raw) = &params.cursor {
        let c = decode(raw)?;
        if c.query != query || c.candidates != candidates || c.offset > found.services.len() {
            return Err(invalid(
                "Service cursor expired: query, scope or candidate set changed; restart the query",
            ));
        }
        c.offset
    } else {
        0
    };
    let enumeration_failed = !found.errors.is_empty();
    let mut out = ServiceStatusOutput {
        scope: found.scope,
        collected_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        complete: !enumeration_failed,
        unknown_count: 0,
        errors: found.errors.into_iter().take(8).collect(),
        ..Default::default()
    };
    for (i, mut s) in found.services.into_iter().enumerate().skip(start) {
        if let Err(error) = enrich(&mut s) {
            out.complete = false;
            out.errors.push(error);
            out.next_cursor = None;
            return Ok(out);
        }
        if serde_json::to_vec(&s)
            .map_err(|e| invalid(e.to_string()))?
            .len()
            > MAX_SERVICE_PAGE_BYTES - 4096
        {
            s.display_name = None;
            s.policy_summary = None;
            s.metadata_error.get_or_insert_with(|| diagnostic(DiagnosticStage::ServiceConfiguration, "service entry budget", "Optional display metadata exceeded the page budget and was omitted; identity and native filter values are unchanged"));
        }
        if s.metadata_error.is_some() {
            out.unknown_count += 1;
            out.complete = false;
            if out.unknown_count == 1 {
                out.errors.push(diagnostic(DiagnosticStage::ServiceConfiguration, "service metadata", "Some inspected service configurations are unknown; no matches does not establish absence"));
            }
        }
        if matches_type(&s, params) != Some(true) {
            continue;
        }
        // Reserve a maximum-size cursor before accepting a whole entry.
        let next = encode(&Cursor {
            query: query.clone(),
            candidates: candidates.clone(),
            offset: i,
        });
        let full = out.services.len() >= params.limit as usize;
        if !full {
            out.services.push(s);
        }
        out.next_cursor = Some(next);
        let oversized = serde_json::to_vec(&out)
            .map_err(|e| invalid(e.to_string()))?
            .len()
            > MAX_SERVICE_PAGE_BYTES - 2048;
        if full || oversized {
            if oversized && !full {
                out.services.pop();
            }
            if out.services.is_empty() {
                return Err(invalid(
                    "Service entry exceeds the JSON page size budget; narrow the query",
                ));
            }
            out.truncated = true;
            if enumeration_failed {
                out.next_cursor = None;
            }
            return Ok(out);
        }
        out.next_cursor = None;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entries() -> Enumeration {
        Enumeration {
            services: (0..1200)
                .map(|i| ServiceEntry {
                    name: format!("service{i:04}"),
                    start_type: Some(if i < 1100 { "manual" } else { "auto" }.into()),
                    scope: "system".into(),
                    ..Default::default()
                })
                .collect(),
            scope: "system".into(),
            ..Default::default()
        }
    }
    #[test]
    fn pages_matches_after_many_nonmatches_without_empty_continuations() {
        let mut p = ServiceStatusParams {
            start_types: vec!["auto".into()],
            limit: 50,
            ..Default::default()
        };
        let a = page(entries(), &p).unwrap();
        assert_eq!(a.services.len(), 50);
        assert_eq!(a.services[0].name, "service1100");
        p.cursor = a.next_cursor;
        let b = page(entries(), &p).unwrap();
        assert_eq!(b.services[0].name, "service1150");
        assert_eq!(b.services.len(), 50);
        assert!(b.next_cursor.is_none());
        p.cursor = None;
        p.start_types = vec!["disabled".into()];
        let empty = page(entries(), &p).unwrap();
        assert!(empty.services.is_empty() && empty.next_cursor.is_none() && empty.complete);
    }
    #[test]
    fn oversized_optional_metadata_is_reduced_without_cutting_identity_or_json() {
        let mut found = entries();
        found.services.truncate(1);
        found.services[0].display_name = Some("说明".repeat(20_000));
        let page = page(
            found,
            &ServiceStatusParams {
                allow_unfiltered: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.services[0].name, "service0000");
        assert_eq!(page.services[0].start_type.as_deref(), Some("manual"));
        assert!(page.services[0].display_name.is_none());
        assert!(!page.complete);
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_SERVICE_PAGE_BYTES);
    }
    #[test]
    fn enrichment_stops_after_lookahead_and_budget_error_retains_matches() {
        let p = ServiceStatusParams {
            allow_unfiltered: true,
            limit: 2,
            ..Default::default()
        };
        let mut inspected = 0;
        let first = page_with(entries(), &p, |_| {
            inspected += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(inspected, 3);
        assert!(first.next_cursor.is_some());
        let mut inspected = 0;
        let incomplete = page_with(entries(), &p, |_| {
            inspected += 1;
            if inspected == 3 {
                Err(diagnostic(
                    DiagnosticStage::Query,
                    "test budget",
                    "query incomplete",
                ))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(incomplete.services.len(), 2);
        assert!(!incomplete.complete && incomplete.next_cursor.is_none());
        assert_eq!(incomplete.unknown_count, 0);
        assert!(serde_json::to_vec(&incomplete).unwrap().len() <= MAX_SERVICE_PAGE_BYTES);
    }
    #[test]
    fn rejects_tampering_and_changed_candidates() {
        let mut p = ServiceStatusParams {
            allow_unfiltered: true,
            limit: 1,
            ..Default::default()
        };
        p.cursor = page(entries(), &p).unwrap().next_cursor;
        let mut changed = entries();
        changed.services.pop();
        assert!(page(changed, &p).is_err());
        p.cursor.as_mut().unwrap().push('x');
        assert!(page(entries(), &p).is_err());
    }
    #[test]
    fn scan_failure_retains_results_without_normal_cursor() {
        let mut found = entries();
        found.errors.push(diagnostic(
            DiagnosticStage::Query,
            "enumerate",
            "query incomplete",
        ));
        let r = page(
            found,
            &ServiceStatusParams {
                allow_unfiltered: true,
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!r.complete && r.next_cursor.is_none() && !r.services.is_empty());
    }
}
