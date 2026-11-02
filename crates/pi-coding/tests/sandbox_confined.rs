#![cfg(target_os = "linux")]

#[path = "sandbox_smoke/cases.rs"]
mod cases;

struct TestCase {
    name: &'static str,
    run: fn() -> std::pin::Pin<Box<dyn Future<Output = cases::ConfinedCaseOutcome>>>,
}

async fn confined_launcher_gate_classifies_absent_vs_broken() -> cases::ConfinedCaseOutcome {
    assert_eq!(
        cases::classify_confined_launcher_gate(
            false,
            Some("trusted Bubblewrap launcher is unavailable"),
        ),
        cases::ConfinedLauncherGate::Skip(cases::BUBBLEWRAP_NOT_INSTALLED)
    );
    assert_eq!(
        cases::classify_confined_launcher_gate(
            false,
            Some("sandbox launcher probe failed: /usr/bin/bwrap: No such file or directory"),
        ),
        cases::ConfinedLauncherGate::Skip(cases::BUBBLEWRAP_NOT_INSTALLED)
    );
    assert_eq!(
        cases::classify_confined_launcher_gate(
            false,
            Some("trusted Bubblewrap launcher validation failed: probe rejected"),
        ),
        cases::ConfinedLauncherGate::Fail(
            "trusted Bubblewrap launcher validation failed: probe rejected".to_owned()
        )
    );
    assert_eq!(
        cases::classify_confined_launcher_gate(true, Some("ignored launcher error")),
        cases::ConfinedLauncherGate::Skip(cases::FORCED_UNSUPPORTED)
    );
    cases::ConfinedCaseOutcome::Passed
}

fn test_cases() -> [TestCase; 12] {
    [
        TestCase {
            name: "confined_launcher_gate_classifies_absent_vs_broken",
            run: || Box::pin(confined_launcher_gate_classifies_absent_vs_broken()),
        },
        TestCase {
            name: "confined_workspace_write_writes_workspace_and_denies_outside",
            run: || Box::pin(cases::confined_workspace_write_writes_workspace_and_denies_outside()),
        },
        TestCase {
            name: "confined_read_only_mode_allows_reads_and_blocks_writes",
            run: || Box::pin(cases::confined_read_only_mode_allows_reads_and_blocks_writes()),
        },
        TestCase {
            name: "confined_bash_pty_redirects_home_and_tmpdir_to_private_projection",
            run: || {
                Box::pin(cases::confined_bash_pty_redirects_home_and_tmpdir_to_private_projection())
            },
        },
        TestCase {
            name: "confined_bash_pty_network_is_off_by_default",
            run: || Box::pin(cases::confined_bash_pty_network_is_off_by_default()),
        },
        TestCase {
            name: "confined_hostile_absolute_denied_path_is_literal_and_never_evaluated",
            run: || {
                Box::pin(
                    cases::confined_hostile_absolute_denied_path_is_literal_and_never_evaluated(),
                )
            },
        },
        TestCase {
            name: "confined_settings_reload_swaps_generation_mode_and_governs_tools",
            run: || {
                Box::pin(cases::confined_settings_reload_swaps_generation_mode_and_governs_tools())
            },
        },
        TestCase {
            name: "confined_bash_timeout_reaps_descendants",
            run: || Box::pin(cases::confined_bash_timeout_reaps_descendants()),
        },
        TestCase {
            name: "confined_bash_foreground_uses_governed_subprocess_authority",
            run: || Box::pin(cases::confined_bash_foreground_uses_governed_subprocess_authority()),
        },
        TestCase {
            name: "confined_bash_tool_bypasses_brush_lock_serialization",
            run: || {
                Box::pin(cases::confined_bash_tool_bypasses_brush_lock_serialization())
            },
        },
        TestCase {
            name: "confined_bash_foreground_fallback_timeout_reaps_descendants",
            run: || Box::pin(cases::confined_bash_foreground_fallback_timeout_reaps_descendants()),
        },
        TestCase {
            name: "governed_subagent_child_inherits_parent_generation_and_stays_confined",
            run: || {
                Box::pin(
                    cases::governed_subagent_child_inherits_parent_generation_and_stays_confined(),
                )
            },
        },
    ]
}

fn main() {
    if std::env::args_os()
        .skip(1)
        .eq(["__sandbox-child", "3", "4"])
    {
        match pi_sandbox::exec_child_target(3, 4) {
            Ok(never) => match never {},
            Err(error) => panic!("sandbox child launch failed: {error}"),
        }
    }

    let filters = std::env::args()
        .skip(1)
        .filter(|argument| !argument.starts_with('-'))
        .collect::<Vec<_>>();
    let selected = test_cases()
        .into_iter()
        .filter(|case| filters.iter().all(|filter| case.name.contains(filter)))
        .collect::<Vec<_>>();
    let selected_count = selected.len();
    if selected_count == 0 {
        eprintln!("no confined sandbox tests matched: {}", filters.join(", "));
        std::process::exit(101);
    }
    println!("running {selected_count} tests");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create confined sandbox test runtime");
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for case in selected {
        print!("test {} ... ", case.name);
        std::io::Write::flush(&mut std::io::stdout()).expect("flush confined test header");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on((case.run)())
        }));
        match outcome {
            Ok(cases::ConfinedCaseOutcome::Passed) => {
                println!("ok");
                passed += 1;
            }
            Ok(cases::ConfinedCaseOutcome::Skipped(reason)) => {
                println!("ignored, SKIPPED: {reason}");
                skipped += 1;
            }
            Err(_) => {
                println!("FAILED");
                failed += 1;
            }
        }
    }
    assert_eq!(
        passed + skipped + failed,
        selected_count,
        "every selected confined case must pass, skip, or fail"
    );
    if failed == 0 {
        println!("\ntest result: ok. {passed} passed; 0 failed; {skipped} ignored");
        return;
    }
    println!("\ntest result: FAILED. {passed} passed; {failed} failed; {skipped} ignored");
    std::process::exit(101);
}
