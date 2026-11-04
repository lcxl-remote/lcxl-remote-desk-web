//! Explicit release-only measurements of registration, publication and cleanup.
use super::{DeviceCodeLifecycle, fixtures::publish};
use crate::{config::connection::DatabaseConnection, entity::device_code};
use actix_web::web;
use desk_signal_facade::model::connection::SharedConnectionMap;
use desk_utils::error::DeskErrorCode;
use futures_util::{StreamExt, stream};
use sea_orm::{ConnectOptions, ConnectionTrait, EntityTrait, Schema, Set};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

fn distribution(mut samples: Vec<u64>) -> serde_json::Value {
    samples.sort_unstable();
    serde_json::json!({"n":samples.len(),"p50_ns":samples[samples.len()/2],"p95_ns":samples[samples.len()*95/100],"p99_ns":samples[samples.len()*99/100]})
}

fn resources(label: &str) -> serde_json::Value {
    let pid = std::process::id().to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=,time=", "-p", &pid])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stats = String::from_utf8(output.stdout).unwrap();
    let stats: Vec<_> = stats.split_whitespace().collect();
    let handles = std::process::Command::new("lsof")
        .args(["-p", &pid, "-Ff"])
        .output()
        .unwrap();
    assert!(handles.status.success());
    let fds = String::from_utf8(handles.stdout)
        .unwrap()
        .lines()
        .filter(|line| {
            line.strip_prefix('f')
                .is_some_and(|value| value.parse::<u32>().is_ok())
        })
        .count();
    serde_json::json!({"phase":label,"rss_kib":stats[0].parse::<u64>().unwrap(),"process_cpu":stats[1],"open_numeric_fds":fds})
}

async fn directory_cost() -> Vec<serde_json::Value> {
    let mut reports = Vec::new();
    for count in [1, 1000, 10000] {
        let queries = Arc::new(AtomicU64::new(0));
        let observed = queries.clone();
        let mut options = ConnectOptions::new("sqlite::memory:");
        options.max_connections(1).sqlx_logging(false);
        let mut raw = sea_orm::Database::connect(options).await.unwrap();
        raw.set_metric_callback(move |info| {
            assert!(!info.failed);
            observed.fetch_add(1, Ordering::Relaxed);
        });
        let db = DatabaseConnection::new(raw, crate::config::test_support::context().await);
        db.execute(
            &Schema::new(db.get_database_backend()).create_table_from_entity(device_code::Entity),
        )
        .await
        .unwrap();
        let now = chrono::Utc::now();
        for start in (0..count).step_by(100) {
            device_code::Entity::insert_many((start..(start + 100).min(count)).map(|i| {
                device_code::ActiveModel {
                    client_id: Set(format!("host-{i}")),
                    device_code: Set(format!("CODE{i}")),
                    created_at: Set(now),
                    updated_at: Set(now),
                    ..Default::default()
                }
            }))
            .exec(&db)
            .await
            .unwrap();
        }
        let map = web::Data::new(SharedConnectionMap::new_oss());
        let lifecycle = Arc::new(DeviceCodeLifecycle::default());
        let (response, handler) = publish(map.clone(), "host-0", "online", "CODE0".into()).await;
        let mut cases = Vec::new();
        for label in [
            "online",
            "code_redemption_lookup",
            "known_offline",
            "unknown",
        ] {
            if label == "known_offline" {
                map.write().await.remove("online");
            }
            let target = if label == "unknown" {
                "missing"
            } else {
                "host-0"
            };
            let mut samples = Vec::new();
            let mut bytes = 0;
            let mut code = 0;
            for sample in 0..120 {
                if sample == 20 {
                    queries.store(0, Ordering::Relaxed);
                }
                let started = Instant::now();
                if label == "code_redemption_lookup" {
                    let resolved = crate::device_code_store::resolve_online(&db, &map, "CODE0")
                        .await
                        .unwrap()
                        .unwrap();
                    let elapsed = started.elapsed().as_nanos() as u64;
                    assert_eq!(resolved.0, "online");
                    assert_eq!(resolved.1.client_id, "host-0");
                    if sample >= 20 {
                        samples.push(elapsed);
                    }
                    continue;
                }
                let response =
                    crate::controller::connection::device_connection(&db, &map, &lifecycle, target)
                        .await;
                assert_eq!(response.status(), 200);
                assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
                let body = actix_web::body::to_bytes(response.into_body())
                    .await
                    .unwrap();
                let elapsed = started.elapsed().as_nanos() as u64;
                let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
                code = json["code"].as_i64().unwrap();
                if label == "online" {
                    assert_eq!(code, 0);
                } else if label == "unknown" {
                    assert_eq!(code, i64::from(DeskErrorCode::DEVICE_NOT_FOUND.code()));
                } else {
                    assert!(
                        [
                            i64::from(DeskErrorCode::DEVICE_NOT_FOUND.code()),
                            i64::from(DeskErrorCode::REMOTE_DESK_OFFLINE.code())
                        ]
                        .contains(&code)
                    );
                }
                if sample >= 20 {
                    samples.push(elapsed);
                    bytes = body.len();
                }
            }
            cases.push(serde_json::json!({"workflow":label,"samples":100,"warmup":20,"latency":distribution(samples),"response_bytes":bytes,"sql_statements_per_sample":queries.load(Ordering::Relaxed)/100,"business_code":code}));
        }
        drop((response, handler));
        assert!(lifecycle.slots.lock().unwrap().is_empty());
        reports.push(serde_json::json!({"registered_devices":count,"cases":cases}));
    }
    reports
}

#[actix_web::test]
#[ignore = "Explicit release performance evidence; run alone with process inspection permission"]
async fn registration_and_cleanup_performance() {
    assert!(!cfg!(debug_assertions), "use --release");
    const SAMPLES: usize = 1024;
    let mut cases = Vec::new();
    let mut resource_curve = vec![resources("initial")];
    for concurrency in [1, 16, 64] {
        for workload in ["same_identity", "different_identities", "new_identities"] {
            let directory = tempfile::tempdir().unwrap();
            let url = format!(
                "sqlite://{}?mode=rwc",
                directory.path().join("codes.db").display()
            );
            let mut options = ConnectOptions::new(url);
            options
                .max_connections(1)
                .min_connections(1)
                .sqlx_logging(false);
            options.map_sqlx_sqlite_opts(|options| {
                options
                    .journal_mode(sea_orm::sqlx::sqlite::SqliteJournalMode::Wal)
                    .synchronous(sea_orm::sqlx::sqlite::SqliteSynchronous::Full)
                    .busy_timeout(Duration::from_secs(5))
            });
            let mut raw = sea_orm::Database::connect(options).await.unwrap();
            let queries = Arc::new(AtomicU64::new(0));
            let db_time = Arc::new(AtomicU64::new(0));
            let (q, t) = (queries.clone(), db_time.clone());
            raw.set_metric_callback(move |info| {
                assert!(!info.failed);
                q.fetch_add(1, Ordering::Relaxed);
                t.fetch_add(info.elapsed.as_nanos() as u64, Ordering::Relaxed);
            });
            let db = DatabaseConnection::new(raw, crate::config::test_support::context().await);
            db.execute(
                &Schema::new(db.get_database_backend())
                    .create_table_from_entity(device_code::Entity),
            )
            .await
            .unwrap();
            let now = chrono::Utc::now();
            for batch in 0..10 {
                device_code::Entity::insert_many((0..100).map(|i| {
                    let i = batch * 100 + i;
                    device_code::ActiveModel {
                        client_id: Set(format!("host-{i}")),
                        device_code: Set(format!("CODE{i}")),
                        created_at: Set(now),
                        updated_at: Set(now),
                        ..Default::default()
                    }
                }))
                .exec(&db)
                .await
                .unwrap();
            }
            let lifecycle = Arc::new(DeviceCodeLifecycle::default());
            let map = web::Data::new(SharedConnectionMap::new_oss());
            let mut base = Vec::new();
            for i in 0..1000 {
                base.push(
                    publish(
                        map.clone(),
                        &format!("host-{i}"),
                        &format!("base-{i}"),
                        format!("CODE{i}"),
                    )
                    .await,
                );
            }
            assert_eq!(map.read().await.len(), 1000);
            // Warm the same paths without including fixture setup in results.
            for i in 0..64 {
                let client = format!("host-{i}");
                let guard = lifecycle.lock(&client).await;
                let code = crate::device_code_store::get_or_create(&db, &client)
                    .await
                    .unwrap();
                base.push(publish(map.clone(), &client, &format!("warm-{i}"), code).await);
                drop(guard);
            }
            queries.store(0, Ordering::Relaxed);
            db_time.store(0, Ordering::Relaxed);
            map.take_publication_timings();
            let began = Instant::now();
            let results: Vec<_> = stream::iter(0..SAMPLES)
                .map(|i| {
                    let (map, db, lifecycle) = (map.clone(), db.clone(), lifecycle.clone());
                    async move {
                        let client = match workload {
                            "same_identity" => "host-0".to_owned(),
                            "different_identities" => format!("host-{}", i % 1000),
                            _ => format!("fresh-{i}"),
                        };
                        let start = Instant::now();
                        let guard = lifecycle.lock(&client).await;
                        let wait = start.elapsed().as_nanos() as u64;
                        let held = Instant::now();
                        let code = crate::device_code_store::get_or_create(&db, &client)
                            .await
                            .unwrap();
                        let published =
                            publish(map.clone(), &client, &format!("sample-{i}"), code).await;
                        let hold = held.elapsed().as_nanos() as u64;
                        drop(guard);
                        (start.elapsed().as_nanos() as u64, wait, hold, published)
                    }
                })
                .buffer_unordered(concurrency)
                .collect()
                .await;
            let elapsed = began.elapsed();
            let query_count = queries.load(Ordering::Relaxed);
            assert_eq!(
                query_count,
                SAMPLES as u64 * if workload == "new_identities" { 2 } else { 1 }
            );
            let lock = map.take_publication_timings();
            assert_eq!(lock.len(), SAMPLES);
            let expected = if workload == "new_identities" {
                1000 + SAMPLES
            } else {
                1000
            };
            assert_eq!(map.read().await.len(), expected);
            for (_, _, _, (_, handler)) in &results {
                let state = &handler.connection_state;
                let registry = map.read().await;
                if !state.lifecycle.is_retired() {
                    assert_eq!(
                        registry
                            .current_oss_server(state.client_id.as_deref().unwrap())
                            .unwrap()
                            .model
                            .connection_id,
                        state.model.connection_id
                    );
                }
            }
            cases.push(serde_json::json!({"concurrency":concurrency,"workload":workload,"warmup":64,"samples":SAMPLES,"elapsed_ns":elapsed.as_nanos(),"registrations_per_second":SAMPLES as f64/elapsed.as_secs_f64(),"latency":distribution(results.iter().map(|v|v.0).collect()),"identity_wait":distribution(results.iter().map(|v|v.1).collect()),"identity_hold_including_db":distribution(results.iter().map(|v|v.2).collect()),"publication_lock_wait":distribution(lock.iter().map(|v|v.0).collect()),"publication_lock_hold":distribution(lock.iter().map(|v|v.1).collect()),"sql_statements":query_count,"sql_elapsed_sum_ns_including_pool_wait":db_time.load(Ordering::Relaxed),"published_current_connections":expected}));
            let released: Vec<_> = base
                .iter()
                .map(|(_, handler)| Arc::downgrade(&handler.connection_state.lifecycle))
                .chain(results.iter().map(|(_, _, _, (_, handler))| {
                    Arc::downgrade(&handler.connection_state.lifecycle)
                }))
                .collect();
            drop(results);
            drop(base);
            tokio::time::timeout(Duration::from_secs(5), async {
                while !map.read().await.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            // Removal precedes transport cleanup. Do not let a previous case's
            // closing tasks contaminate the next case's resource measurements.
            tokio::time::timeout(Duration::from_secs(3), async {
                while released.iter().any(|state| state.strong_count() != 0) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("all published connection objects must be released");
            assert!(lifecycle.slots.lock().unwrap().is_empty());
            db.close().await.unwrap();
            resource_curve.push(resources(&format!("idle-{concurrency}-{workload}")));
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    resource_curve.push(resources("final-idle"));
    let directory = directory_cost().await;
    println!(
        "REGISTRATION_COST {}",
        serde_json::json!({"profile":"release","boundary":"real SQLite WAL/FULL single pool + real WS session/SignalingHandler::init; in-process scheduling; no network/media","cases":cases,"directory":directory,"resources":resource_curve})
    );
}
