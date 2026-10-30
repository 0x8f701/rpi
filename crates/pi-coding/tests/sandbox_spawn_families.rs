//! Closed 19-family spawn coverage proof (D6 integration file).
//!
//! The product's spawn coverage is a closed enumeration owned by
//! `pi-sandbox`: `SpawnFamily::ALL` has exactly 19 families and the
//! `covers(route, family)` matrix is the single authoritative coverage table
//! (C14, §13.1). Settings, RPC, approval, extensions, and callers cannot add
//! a family or widen a route. These tests prove that closed set and that the
//! product's credential-audience surface can express every non-McpStdio
//! family (McpStdio is the one family that must use the MCP-server audience,
//! never a spawn-family audience).
//!
//! Pure static/coverage proofs: no process is spawned and no launcher is
//! probed, so the file is deterministic on every host (including the
//! `sandbox-static-gates` job, which runs this file without installing
//! bubblewrap).

use std::collections::BTreeSet;

use pi_coding::{
    CredentialAudience, CredentialConfigError, CredentialGrantDefinition,
    CredentialGrantOutputConfig,
};
use pi_sandbox::{CoverageRoute, SpawnFamily, covers};

/// The family set is closed at exactly nineteen, each family has a unique
/// stable kebab-case name, and `as_str` round-trips through the serde wire
/// format used by settings (`spawnFamily` audiences) and by the raw-spawn
/// gate. A collapsed/duplicated/renamed family or a wire name that no longer
/// round-trips fails here.
#[test]
fn spawn_family_set_is_closed_at_exactly_nineteen() {
    assert_eq!(
        SpawnFamily::ALL.len(),
        19,
        "the spawn-family coverage must stay closed at exactly 19 families"
    );

    let mut names = BTreeSet::new();
    for family in SpawnFamily::ALL {
        let name = family.as_str();
        // Unique, stable, lowercase kebab-case identifier.
        assert!(
            names.insert(name),
            "family {name:?} is duplicated in SpawnFamily::ALL"
        );
        assert!(!name.is_empty(), "family {family:?} has an empty name");
        assert!(
            name.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'),
            "family name {name:?} is not lowercase kebab-case"
        );
        assert!(
            !name.starts_with('-') && !name.ends_with('-') && !name.contains("--"),
            "family name {name:?} is not well-formed kebab-case"
        );

        // The serde wire name must exactly equal `as_str` in both directions.
        let serialized = serde_json::to_string(&family).expect("serialize family");
        assert_eq!(
            serialized,
            format!("\"{name}\""),
            "serde wire name of {family:?} must equal as_str()"
        );
        let deserialized: SpawnFamily =
            serde_json::from_str(&serialized).expect("deserialize family");
        assert_eq!(deserialized, family, "{name} must round-trip through serde");
    }
    // Ord must be consistent so coverage tables and settings catalogs can rely
    // on the canonical order.
    assert!(SpawnFamily::ALL.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(names.len(), 19);
}

/// The authoritative 6-route x 19-family coverage matrix is exact:
///
/// - Linux confined, Linux danger, and macOS danger cover all 19 families;
/// - macOS confined and Windows danger cover exactly the 18 non-Browser
///   families (Browser is unavailable there);
/// - Windows confined covers nothing (Windows makes no confinement claim).
///
/// A family added to `ALL` without a route, a route widened to Browser on
/// macOS/Windows, or a Windows confined route that suddenly claims coverage
/// fails this literal matrix.
#[test]
fn coverage_matrix_is_exact_for_every_family_and_route() {
    let all_routes = [
        CoverageRoute::LinuxConfined,
        CoverageRoute::MacosConfined,
        CoverageRoute::WindowsConfined,
        CoverageRoute::LinuxDanger,
        CoverageRoute::MacosDanger,
        CoverageRoute::WindowsDanger,
    ];

    for family in SpawnFamily::ALL {
        for route in all_routes {
            let covered = covers(route, family);
            let expected = match route {
                CoverageRoute::LinuxConfined
                | CoverageRoute::LinuxDanger
                | CoverageRoute::MacosDanger => true,
                CoverageRoute::MacosConfined | CoverageRoute::WindowsDanger => {
                    family != SpawnFamily::Browser
                }
                CoverageRoute::WindowsConfined => false,
            };
            assert_eq!(
                covered,
                expected,
                "coverage mismatch for family {} on route {route:?}",
                family.as_str()
            );
        }
    }

    // Every family has at least one authoritative route (nothing is
    // unreachable) and Windows confined covers none of them.
    for family in SpawnFamily::ALL {
        let covered_routes = all_routes
            .into_iter()
            .filter(|route| covers(*route, family))
            .count();
        assert!(
            covered_routes >= 1,
            "family {} has no covering route",
            family.as_str()
        );
        assert!(
            !covers(CoverageRoute::WindowsConfined, family),
            "family {} must never be covered by Windows confined",
            family.as_str()
        );
    }
}

/// The product's credential-audience surface (`credentialGrants` settings
/// with a `spawnFamily` audience) can express every family except McpStdio.
/// McpStdio is the single family that must use the `mcpServer` audience, so
/// the audience constructor must reject it — and the same rule must hold
/// when the audience travels through a full credential-grant definition
/// (the settings path). A new family that is not expressible, or McpStdio
/// sneaking into the spawn-family audience, fails here.
#[test]
fn every_family_is_reachable_as_a_spawn_family_credential_audience() {
    for family in SpawnFamily::ALL {
        let audience = CredentialAudience::new_spawn_family(family);
        if family == SpawnFamily::McpStdio {
            let error = audience.expect_err("McpStdio must not use a spawn-family audience");
            assert!(
                matches!(error, CredentialConfigError::InvalidMcpAudience),
                "McpStdio rejection must be the typed InvalidMcpAudience error, got {error:?}"
            );
            continue;
        }

        let audience = audience.expect("spawn-family audience");
        let wire = serde_json::to_string(&audience).expect("serialize audience");
        let restored: CredentialAudience =
            serde_json::from_str(&wire).expect("deserialize audience");
        assert_eq!(
            restored,
            audience,
            "{} audience must round-trip through settings serde",
            family.as_str()
        );

        // The full settings path: a credential grant definition carrying the
        // family audience must be constructible (this is what settings.toml /
        // settings.json `credentialGrantDefinitions` deserialize into).
        let environment = pi_sandbox::EnvironmentVariableName::parse("TEST_CREDENTIAL".to_owned())
            .expect("environment name");
        let definition = CredentialGrantDefinition::new(
            pi_sandbox::CredentialGrantId::parse(format!("grant-{}", family.as_str()))
                .expect("grant id"),
            pi_coding::ProviderId::parse("test-provider".to_owned()).expect("provider id"),
            None,
            audience.clone(),
            CredentialGrantOutputConfig::api_key(environment.clone()).expect("output config"),
        )
        .expect("credential grant definition must accept every non-McpStdio family");
        assert_eq!(
            definition.audience().spawn_family(),
            Some(family),
            "definition audience must retain the exact family"
        );
        assert_eq!(
            definition.output().environment(),
            &environment,
            "definition must retain the exact output environment"
        );
    }
}
