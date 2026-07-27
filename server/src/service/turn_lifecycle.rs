//! Whether this process runs a TURN service, and why — plus the runtime
//! parameters that follow from it.
//!
//! Three independent conditions decide it: the startup mode has to be one that
//! hosts signaling at all, the operator has to have left the service enabled,
//! and there has to be an interface to serve on. Keeping them apart matters for
//! what the operator sees — a host that never serves TURN is not the same as one
//! whose TURN was switched off, which in turn is not the same as one that was
//! never given an address, and none of the three is a failure to report.
//!
//! This is the single place the desired runtime state is derived, so the startup
//! path and every later settings write converge on identical parameters; two
//! derivations would eventually disagree and restart the runtime on a save that
//! changed nothing.

use desk_turn::interface::plan_turn_interfaces;
use desk_turn::model::TurnSettings;
use desk_turn::runtime::{TurnIntent, TurnPosture};
use desk_turn::supervisor::{DesiredState, TurnRuntimeParams};

use crate::model::settings::StartupMode;

/// What the host means to do about TURN, and what to hand the supervisor.
pub struct TurnPlan {
    pub posture: TurnPosture,
    pub desired: DesiredState,
}

/// Decide what to run in `mode` under `settings`, tagged with `revision`.
pub fn turn_plan(mode: &StartupMode, settings: &TurnSettings, revision: u64) -> TurnPlan {
    let posture = turn_posture(mode, settings);
    let params = match posture.intent {
        TurnIntent::Run => Some(TurnRuntimeParams {
            realm: settings.realm.clone(),
            secret: settings.static_auth_secret.clone(),
            // Only what will actually be served: an entry the runtime refuses to
            // bind must not count towards "did the configuration change", or
            // correcting a typo that leaves the entry just as unusable would
            // restart a healthy relay for nothing.
            interfaces: plan_turn_interfaces(&settings.interfaces)
                .servable
                .iter()
                .map(|servable| servable.canonical())
                .collect(),
            relay_min_port: settings.relay_min_port,
            relay_max_port: settings.relay_max_port,
            // A host has no control plane to tell about its runtime, so there is
            // nothing an opaque tag would let anyone correlate; the parameters
            // themselves already decide when a restart is due.
            identity: String::new(),
        }),
        TurnIntent::Disabled | TurnIntent::Unsupported | TurnIntent::NotConfigured => None,
    };
    TurnPlan {
        posture,
        desired: DesiredState { revision, params },
    }
}

/// Why this host is or is not meant to relay, and which configured interfaces
/// it will not serve.
///
/// The order is deliberate. `Unsupported` wins over everything: a desk server or
/// a worker would not have served TURN even with the switch on, so reporting it
/// as "switched off" would invite an operator to look for a switch that changes
/// nothing. `Disabled` wins over `NotConfigured` for the same reason — with the
/// service off, a missing interface is not what needs fixing.
///
/// "Configured" means *servable*: an interface the host cannot bind is no more
/// an address to serve on than a missing one, and treating it as one would put
/// the supervisor into a retry loop against a start that can never succeed. The
/// rejections travel with the posture so that case is still distinguishable from
/// having configured nothing at all.
pub fn turn_posture(mode: &StartupMode, settings: &TurnSettings) -> TurnPosture {
    let plan = plan_turn_interfaces(&settings.interfaces);
    let intent = if !mode_hosts_turn(mode) {
        TurnIntent::Unsupported
    } else if !settings.enable_turn {
        TurnIntent::Disabled
    } else if plan.servable.is_empty() {
        TurnIntent::NotConfigured
    } else {
        TurnIntent::Run
    };
    TurnPosture {
        intent,
        rejected_interfaces: plan.rejected,
    }
}

/// Whether a startup mode runs an embedded signaling server, which is what the
/// TURN service belongs to.
pub fn mode_hosts_turn(mode: &StartupMode) -> bool {
    match mode {
        StartupMode::Default | StartupMode::Signaling => true,
        StartupMode::DeskServer
        | StartupMode::ServiceDaemon
        | StartupMode::SessionWorker
        | StartupMode::McpStdio => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_turn::interface::TurnInterfaceFault;
    use desk_turn::model::{TurnInterface, TurnTransport};

    fn iface(listen: &str) -> TurnInterface {
        TurnInterface {
            transport: TurnTransport::UDP,
            listen: listen.into(),
            external: "203.0.113.9:3478".into(),
        }
    }

    fn turn_intent(mode: &StartupMode, settings: &TurnSettings) -> TurnIntent {
        turn_posture(mode, settings).intent
    }

    fn settings(enable_turn: bool, interfaces: usize) -> TurnSettings {
        TurnSettings {
            enable_turn,
            interfaces: (0..interfaces)
                .map(|i| iface(&format!("0.0.0.0:{}", 3478 + i)))
                .collect(),
            ..TurnSettings::default()
        }
    }

    /// The switch only has an answer to give where TURN could run at all, and
    /// where it can, it decides.
    #[test]
    fn every_mode_and_switch_combination_has_a_stable_answer() {
        for mode in [
            StartupMode::Default,
            StartupMode::Signaling,
            StartupMode::DeskServer,
            StartupMode::ServiceDaemon,
            StartupMode::SessionWorker,
            StartupMode::McpStdio,
        ] {
            let hosts = matches!(mode, StartupMode::Default | StartupMode::Signaling);
            let expected_on = if hosts {
                TurnIntent::Run
            } else {
                TurnIntent::Unsupported
            };
            let expected_off = if hosts {
                TurnIntent::Disabled
            } else {
                TurnIntent::Unsupported
            };
            assert_eq!(
                turn_intent(&mode, &settings(true, 1)),
                expected_on,
                "{mode:?}"
            );
            assert_eq!(
                turn_intent(&mode, &settings(false, 1)),
                expected_off,
                "{mode:?}",
            );
        }
    }

    /// Switched on with nowhere to listen is its own answer: the operator has to
    /// add an address, which is a different action from flipping a switch, and
    /// the server refuses to start without one anyway.
    #[test]
    fn an_enabled_service_with_no_interface_is_not_confused_with_a_disabled_one() {
        assert_eq!(
            turn_intent(&StartupMode::Default, &settings(true, 0)),
            TurnIntent::NotConfigured
        );
        assert_eq!(
            turn_intent(&StartupMode::Default, &settings(false, 0)),
            TurnIntent::Disabled,
            "with the service off, a missing address is not the thing to fix"
        );
    }

    /// The default settings leave TURN switched on: the switch decides, and a
    /// host that goes on to configure an interface gets it served without
    /// hunting for a toggle. Out of the box there is no interface yet, so
    /// nothing starts.
    #[test]
    fn a_default_configuration_wants_turn_but_has_nowhere_to_serve_it() {
        assert!(TurnSettings::default().enable_turn);
        let plan = turn_plan(&StartupMode::Default, &TurnSettings::default(), 1);
        assert_eq!(plan.posture.intent, TurnIntent::NotConfigured);
        assert!(plan.desired.params.is_none());
        assert!(plan.posture.rejected_interfaces.is_empty());
    }

    /// A host whose every interface is unservable has nowhere to serve, exactly
    /// like one with no interfaces — the supervisor must not be told to start a
    /// runtime that can only fail, forever.
    ///
    /// The two cases are still told apart, by the rejections travelling with the
    /// posture: "you configured nothing" and "none of your three entries can be
    /// bound" call for different actions.
    #[test]
    fn interfaces_that_cannot_be_bound_are_nowhere_to_serve() {
        let settings = TurnSettings {
            enable_turn: true,
            interfaces: vec![
                TurnInterface {
                    transport: TurnTransport::TCP,
                    listen: "0.0.0.0:3478".into(),
                    external: "203.0.113.9:3478".into(),
                },
                TurnInterface {
                    transport: TurnTransport::UDP,
                    listen: "0.0.0.0:3478".into(),
                    external: "relay.example.com:3478".into(),
                },
            ],
            ..TurnSettings::default()
        };

        let plan = turn_plan(&StartupMode::Default, &settings, 1);
        assert_eq!(plan.posture.intent, TurnIntent::NotConfigured);
        assert!(
            plan.desired.params.is_none(),
            "a start that cannot succeed must not be attempted"
        );
        assert_eq!(
            plan.posture
                .rejected_interfaces
                .iter()
                .map(|r| r.fault)
                .collect::<Vec<_>>(),
            vec![
                TurnInterfaceFault::TransportNotServed,
                TurnInterfaceFault::ExternalNotAnAddress,
            ],
            "an operator who configured two entries must learn why neither is used"
        );
    }

    /// A configuration that is partly wrong still relays on what is right, and
    /// the runtime is only told about the entries it can bind — advertising the
    /// rest would point peers at addresses nothing listens on.
    #[test]
    fn a_partly_wrong_configuration_serves_the_rest() {
        let mut settings = settings(true, 1);
        settings.interfaces.push(TurnInterface {
            transport: TurnTransport::TCP,
            listen: "0.0.0.0:3478".into(),
            external: "203.0.113.9:3478".into(),
        });

        let plan = turn_plan(&StartupMode::Default, &settings, 1);
        assert_eq!(plan.posture.intent, TurnIntent::Run);
        let params = plan.desired.params.expect("the UDP entry is servable");
        assert_eq!(
            params.interfaces,
            vec![iface("0.0.0.0:3478")],
            "the TCP entry reaches neither the sockets nor the advertisement"
        );
        assert_eq!(plan.posture.rejected_interfaces.len(), 1);
    }

    /// Every configured interface reaches the runtime: dropping all but the
    /// first would silently unserve addresses the operator asked for.
    #[test]
    fn the_plan_carries_the_whole_configuration() {
        let mut settings = settings(true, 2);
        settings.realm = "relay.example".into();
        settings.static_auth_secret = Some("s3cret".into());
        settings.relay_min_port = 40000;
        settings.relay_max_port = 40100;

        let plan = turn_plan(&StartupMode::Signaling, &settings, 7);
        assert_eq!(plan.posture.intent, TurnIntent::Run);
        assert_eq!(plan.desired.revision, 7);
        let params = plan.desired.params.expect("an enabled host runs a runtime");
        assert_eq!(params.realm, "relay.example");
        assert_eq!(params.secret.as_deref(), Some("s3cret"));
        assert_eq!(params.interfaces, settings.interfaces);
        assert_eq!(params.relay_min_port, 40000);
        assert_eq!(params.relay_max_port, 40100);
    }

    /// Restarts are driven by parameter equality, so an unrelated save must not
    /// produce different parameters — and a change that matters must.
    #[test]
    fn only_a_real_configuration_change_produces_different_parameters() {
        let base = settings(true, 1);
        let a = turn_plan(&StartupMode::Default, &base, 1).desired.params;
        let b = turn_plan(&StartupMode::Default, &base, 2).desired.params;
        assert_eq!(a, b, "the same configuration must not force a restart");

        let mut rotated = base.clone();
        rotated.static_auth_secret = Some("rotated".into());
        assert_ne!(
            turn_plan(&StartupMode::Default, &rotated, 3).desired.params,
            a,
            "a rotated secret must restart the runtime that signs with it"
        );

        let mut added = base.clone();
        added.interfaces.push(iface("0.0.0.0:3479"));
        assert_ne!(
            turn_plan(&StartupMode::Default, &added, 4).desired.params,
            a,
            "an added interface must reach a runtime"
        );

        // Editing an entry that stays unservable changes nothing the runtime
        // could act on, so it must not tear down a working relay.
        let mut unservable = base.clone();
        unservable.interfaces.push(TurnInterface {
            transport: TurnTransport::UDP,
            listen: "0.0.0.0:3480".into(),
            external: "typo.example.com:3478".into(),
        });
        let with_typo = turn_plan(&StartupMode::Default, &unservable, 5)
            .desired
            .params;
        assert_eq!(with_typo, a, "an unservable entry is not a configuration");
        unservable.interfaces.last_mut().unwrap().external = "still-a-hostname:3478".into();
        assert_eq!(
            turn_plan(&StartupMode::Default, &unservable, 6)
                .desired
                .params,
            with_typo,
            "correcting one unservable entry into another must not restart the relay"
        );
    }
}
