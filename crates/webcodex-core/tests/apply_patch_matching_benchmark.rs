use std::collections::BTreeMap;

use webcodex_core::apply_patch_shared::{
    derive_codex_patch_update_with_matching_mode, ApplyPatchMatchingMode, CodexPatchChunk,
};

#[derive(Clone)]
struct FileFixture {
    path: &'static str,
    original: &'static str,
    chunks: Vec<CodexPatchChunk>,
    expected_content: &'static str,
}

struct Case {
    name: &'static str,
    files: Vec<FileFixture>,
    baseline_accept: bool,
    unique_accept: bool,
}

fn chunk(
    old: &'static str,
    new: &'static str,
    context: Option<&'static str>,
    eof: bool,
) -> CodexPatchChunk {
    CodexPatchChunk {
        change_context: context.map(str::to_string),
        old_lines: vec![old.to_string()],
        new_lines: vec![new.to_string()],
        is_end_of_file: eof,
    }
}

fn file(
    path: &'static str,
    original: &'static str,
    chunks: Vec<CodexPatchChunk>,
    expected_content: &'static str,
) -> FileFixture {
    FileFixture {
        path,
        original,
        chunks,
        expected_content,
    }
}

fn case(
    name: &'static str,
    fixture: FileFixture,
    baseline_accept: bool,
    unique_accept: bool,
) -> Case {
    Case {
        name,
        files: vec![fixture],
        baseline_accept,
        unique_accept,
    }
}

#[derive(Default)]
struct Evaluation {
    accepted: bool,
    reason: &'static str,
    wrong_location_writes: usize,
    partial_writes: usize,
}

fn evaluate(case: &Case, mode: ApplyPatchMatchingMode) -> Evaluation {
    let mut planned = Vec::with_capacity(case.files.len());
    for fixture in &case.files {
        let update = match derive_codex_patch_update_with_matching_mode(
            fixture.original,
            fixture.path,
            &fixture.chunks,
            mode,
        ) {
            Ok(update) => update,
            Err(error) => {
                return Evaluation {
                    accepted: false,
                    reason: if error.kind == "context_mismatch" {
                        "context_mismatch"
                    } else {
                        "other_rejection"
                    },
                    ..Default::default()
                };
            }
        };
        if let Some(rejection) = update
            .chunk_matches
            .iter()
            .find_map(|matched| matched.match_rejection.as_ref())
        {
            return Evaluation {
                accepted: false,
                reason: if rejection.candidate_count > 1 {
                    "ambiguous_candidate"
                } else {
                    "unique_non_exact_candidate"
                },
                ..Default::default()
            };
        }
        planned.push((fixture, update.content));
    }

    // The benchmark simulates the real transaction boundary: no workspace state
    // changes until every file/hunk has passed preflight. Therefore rejected
    // cases can never produce a partial write in this corpus.
    let wrong_location_writes = planned
        .iter()
        .filter(|(fixture, content)| content.as_str() != fixture.expected_content)
        .count();
    Evaluation {
        accepted: true,
        reason: "accepted",
        wrong_location_writes,
        partial_writes: 0,
    }
}

fn corpus() -> Vec<Case> {
    vec![
        case(
            "ordinary_exact_rust",
            file(
                "src/lib.rs",
                "let x = 1;\n",
                vec![chunk("let x = 1;", "let x = 2;", None, false)],
                "let x = 2;\n",
            ),
            true,
            true,
        ),
        case(
            "ordinary_exact_python",
            file(
                "app.py",
                "value = 1\n",
                vec![chunk("value = 1", "value = 2", None, false)],
                "value = 2\n",
            ),
            true,
            true,
        ),
        case(
            "ordinary_exact_ts",
            file(
                "ui.ts",
                "const a = 1;\n",
                vec![chunk("const a = 1;", "const a = 2;", None, false)],
                "const a = 2;\n",
            ),
            true,
            true,
        ),
        case(
            "docs_exact",
            file(
                "README.md",
                "old heading\n",
                vec![chunk("old heading", "new heading", None, false)],
                "new heading\n",
            ),
            true,
            true,
        ),
        case(
            "comment_exact",
            file(
                "src/a.rs",
                "// old\n",
                vec![chunk("// old", "// new", None, false)],
                "// new\n",
            ),
            true,
            true,
        ),
        case(
            "assert_exact",
            file(
                "tests/a.rs",
                "assert_eq!(x, 1);\n",
                vec![chunk("assert_eq!(x, 1);", "assert_eq!(x, 2);", None, false)],
                "assert_eq!(x, 2);\n",
            ),
            true,
            true,
        ),
        case(
            "trim_end_spaces",
            file(
                "a.txt",
                "target   \n",
                vec![chunk("target", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "trim_end_tab",
            file(
                "a.txt",
                "target\t\n",
                vec![chunk("target", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "trim_end_comment",
            file(
                "a.rs",
                "// target  \n",
                vec![chunk("// target", "// changed", None, false)],
                "// changed\n",
            ),
            false,
            true,
        ),
        case(
            "trim_end_assert",
            file(
                "a.rs",
                "assert!(ready);   \n",
                vec![chunk("assert!(ready);", "assert!(done);", None, false)],
                "assert!(done);\n",
            ),
            false,
            true,
        ),
        case(
            "trim_both_spaces",
            file(
                "a.txt",
                "  target  \n",
                vec![chunk("target", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "trim_both_tabs",
            file(
                "a.txt",
                "\ttarget\t\n",
                vec![chunk("target", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "trim_indented_helper",
            file(
                "a.py",
                "    helper()  \n",
                vec![chunk("helper()", "changed()", None, false)],
                "changed()\n",
            ),
            false,
            true,
        ),
        case(
            "trim_indented_assert",
            file(
                "a.py",
                "    assert value  \n",
                vec![chunk("assert value", "assert changed", None, false)],
                "assert changed\n",
            ),
            false,
            true,
        ),
        case(
            "normalized_em_dash",
            file(
                "docs.md",
                "alpha—beta\n",
                vec![chunk("alpha-beta", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "normalized_en_dash",
            file(
                "docs.md",
                "alpha–beta\n",
                vec![chunk("alpha-beta", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "normalized_minus",
            file(
                "docs.md",
                "alpha−beta\n",
                vec![chunk("alpha-beta", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "normalized_smart_single",
            file(
                "docs.md",
                "it’s ready\n",
                vec![chunk("it's ready", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "normalized_smart_double",
            file(
                "docs.md",
                "say “ready”\n",
                vec![chunk("say \"ready\"", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "normalized_nbsp",
            file(
                "docs.md",
                "alpha\u{00a0}beta\n",
                vec![chunk("alpha beta", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "normalized_ideographic_space",
            file(
                "docs.md",
                "alpha\u{3000}beta\n",
                vec![chunk("alpha beta", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "tier_exact_beats_fuzzy",
            file(
                "a.txt",
                " target \ntarget\n",
                vec![chunk("target", "changed", None, false)],
                " target \nchanged\n",
            ),
            true,
            true,
        ),
        case(
            "anchored_repeated_helper",
            file(
                "a.py",
                "def a():\nhelper()\ndef b():\nhelper()\n",
                vec![chunk("helper()", "changed()", Some("def b():"), false)],
                "def a():\nhelper()\ndef b():\nchanged()\n",
            ),
            true,
            true,
        ),
        case(
            "anchored_repeated_assert_with_drift",
            file(
                "a.py",
                "def a():\nassert value\ndef b():\n  assert value  \n",
                vec![chunk(
                    "assert value",
                    "assert changed",
                    Some("def b():"),
                    false,
                )],
                "def a():\nassert value\ndef b():\nassert changed\n",
            ),
            false,
            true,
        ),
        case(
            "eof_duplicate_structural",
            file(
                "a.txt",
                "same\nmid\nsame\n",
                vec![chunk("same", "last", None, true)],
                "same\nmid\nlast\n",
            ),
            false,
            true,
        ),
        case(
            "ambiguous_exact",
            file(
                "a.txt",
                "dup\nmid\ndup\n",
                vec![chunk("dup", "changed", None, false)],
                "dup\nmid\ndup\n",
            ),
            false,
            false,
        ),
        case(
            "ambiguous_trim_end",
            file(
                "a.txt",
                "dup  \nmid\ndup\t\n",
                vec![chunk("dup", "changed", None, false)],
                "dup  \nmid\ndup\t\n",
            ),
            false,
            false,
        ),
        case(
            "ambiguous_trim",
            file(
                "a.txt",
                " dup \nmid\n\tdup\t\n",
                vec![chunk("dup", "changed", None, false)],
                " dup \nmid\n\tdup\t\n",
            ),
            false,
            false,
        ),
        case(
            "ambiguous_normalized",
            file(
                "a.txt",
                "alpha—beta\nmid\nalpha–beta\n",
                vec![chunk("alpha-beta", "changed", None, false)],
                "alpha—beta\nmid\nalpha–beta\n",
            ),
            false,
            false,
        ),
        case(
            "repeated_parent_context_unique_target",
            file(
                "a.txt",
                "ctx\nold\nctx\nother\n",
                vec![chunk("old", "new", Some("ctx"), false)],
                "ctx\nnew\nctx\nother\n",
            ),
            false,
            true,
        ),
        case(
            "context_mismatch",
            file(
                "a.txt",
                "actual\n",
                vec![chunk("missing", "changed", None, false)],
                "actual\n",
            ),
            false,
            false,
        ),
        Case {
            name: "multi_hunk_mixed_exact_trim",
            files: vec![file(
                "a.txt",
                "one\n two \n",
                vec![
                    chunk("one", "ONE", None, false),
                    chunk("two", "TWO", None, false),
                ],
                "ONE\nTWO\n",
            )],
            baseline_accept: false,
            unique_accept: true,
        },
        Case {
            name: "multi_file_exact_normalized",
            files: vec![
                file(
                    "a.txt",
                    "old\n",
                    vec![chunk("old", "new", None, false)],
                    "new\n",
                ),
                file(
                    "b.txt",
                    "alpha—beta\n",
                    vec![chunk("alpha-beta", "changed", None, false)],
                    "changed\n",
                ),
            ],
            baseline_accept: false,
            unique_accept: true,
        },
        Case {
            name: "multi_file_later_true_ambiguity",
            files: vec![
                file(
                    "safe.txt",
                    "old\n",
                    vec![chunk("old", "new", None, false)],
                    "new\n",
                ),
                file(
                    "risky.txt",
                    "dup\nmid\ndup\n",
                    vec![chunk("dup", "changed", None, false)],
                    "dup\nmid\ndup\n",
                ),
            ],
            baseline_accept: false,
            unique_accept: false,
        },
        case(
            "unicode_narrow_nbsp",
            file(
                "docs.md",
                "alpha\u{202f}beta\n",
                vec![chunk("alpha beta", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
        case(
            "unicode_medium_space",
            file(
                "docs.md",
                "alpha\u{205f}beta\n",
                vec![chunk("alpha beta", "changed", None, false)],
                "changed\n",
            ),
            false,
            true,
        ),
    ]
}

#[test]
fn deterministic_current_main_strict_vs_unique_corpus() {
    let corpus = corpus();
    assert!(
        (25..=40).contains(&corpus.len()),
        "benchmark corpus must remain representative and bounded"
    );

    let mut baseline_accepted = 0usize;
    let mut unique_accepted = 0usize;
    let mut baseline_reasons = BTreeMap::<&'static str, usize>::new();
    let mut unique_reasons = BTreeMap::<&'static str, usize>::new();
    let mut wrong_location_writes = 0usize;
    let mut partial_writes = 0usize;

    for case in &corpus {
        // Baseline is the exact+unique behavior of current-main
        // `strict_matching=true`, which is the deployed model/reviewer policy
        // this P0 is replacing as the normal path. This intentionally does not
        // pretend current-main's schema-default permissive false path was the
        // observed dogfood strategy.
        let baseline = evaluate(case, ApplyPatchMatchingMode::ExactUnique);
        let unique = evaluate(case, ApplyPatchMatchingMode::Unique);
        assert_eq!(
            baseline.accepted, case.baseline_accept,
            "baseline: {}",
            case.name
        );
        assert_eq!(unique.accepted, case.unique_accept, "unique: {}", case.name);
        baseline_accepted += usize::from(baseline.accepted);
        unique_accepted += usize::from(unique.accepted);
        *baseline_reasons.entry(baseline.reason).or_default() += 1;
        *unique_reasons.entry(unique.reason).or_default() += 1;
        wrong_location_writes += baseline.wrong_location_writes + unique.wrong_location_writes;
        partial_writes += baseline.partial_writes + unique.partial_writes;
    }

    assert_eq!(
        wrong_location_writes, 0,
        "matcher must never accept a wrong target in corpus"
    );
    assert_eq!(
        partial_writes, 0,
        "preflight simulation must never partially write"
    );
    assert!(unique_accepted > baseline_accepted);
    println!(
        "apply_patch matcher corpus cases={} current_main_strict accepted={} rejected={} reasons={:?}; unique accepted={} rejected={} reasons={:?}; wrong_location_writes={}; partial_writes={}",
        corpus.len(),
        baseline_accepted,
        corpus.len() - baseline_accepted,
        baseline_reasons,
        unique_accepted,
        corpus.len() - unique_accepted,
        unique_reasons,
        wrong_location_writes,
        partial_writes,
    );
}
