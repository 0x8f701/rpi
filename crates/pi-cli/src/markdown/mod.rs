//! CLI adapters and contract tests for the shared bounded Markdown renderer.

pub mod ratatui;

pub use pi_coding::markdown::*;

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn render(source: &str, width: usize) -> MarkdownRenderOutput {
        render_markdown(
            source,
            &MarkdownRenderOptions {
                width,
                ..MarkdownRenderOptions::default()
            },
        )
    }

    #[test]
    fn table_alignment_golden() {
        let output = render(
            "| left | middle | right |\n| :--- | :---: | ---: |\n| a | b | c |",
            28,
        );
        assert_eq!(
            output.plain_text(),
            "┌───────┬─────────┬────────┐\n│left   │ middle  │   right│\n├───────┼─────────┼────────┤\n│a      │    b    │       c│\n└───────┴─────────┴────────┘"
        );
    }

    #[test]
    fn table_cjk_and_emoji_use_display_width() {
        let output = render(
            "| 名称 | 状态 |\n| --- | ---: |\n| 東京 | ✅ |\n| rocket | 🚀 |",
            19,
        );
        for line in output.plain_lines() {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 19, "{line:?}");
        }
        assert_eq!(
            output.plain_text(),
            "┌─────────┬───────┐\n│名称     │   状态│\n├─────────┼───────┤\n│東京     │     ✅│\n│rocket   │     🚀│\n└─────────┴───────┘"
        );
    }

    #[test]
    fn table_wraps_at_narrow_width() {
        let output = render(
            "| item | detail |\n| --- | --- |\n| alpha | one two three |",
            13,
        );
        assert_eq!(
            output.plain_text(),
            "┌─────┬─────┐\n│item │detai│\n│     │l    │\n├─────┼─────┤\n│alpha│one  │\n│     │two  │\n│     │three│\n└─────┴─────┘"
        );
        assert!(
            output
                .plain_lines()
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 13)
        );
    }

    #[test]
    fn too_narrow_table_visibly_falls_back_to_source() {
        let source = "| a | b |\n| --- | --- |\n| c | d |";
        let output = render(source, 4);
        assert_eq!(output.plain_text(), source);
        assert!(matches!(
            output.diagnostics.as_slice(),
            [RenderDiagnostic::TableTooNarrow { source_line: 1 }]
        ));
    }

    #[test]
    fn malformed_and_streaming_tables_stay_plain() {
        let malformed = render("| a | b |\n| -- | --- |\n| c | d |", 20);
        assert_eq!(
            malformed.plain_text(),
            "| a | b |\n| -- | --- |\n| c | d |"
        );

        let streaming = render_markdown_streaming(
            "| a | b |\n| --- | --- |",
            &MarkdownRenderOptions {
                width: 20,
                ..MarkdownRenderOptions::default()
            },
        );
        assert_eq!(streaming.plain_text(), "| a | b |\n| --- | --- |");
    }

    #[test]
    fn headings_lists_and_nested_fences_are_analyzed() {
        let document = analyze_markdown(
            "# Header\n\n- [x] done\n  2. next\n\n````markdown\n```mermaid\nflowchart TD\n```\n````",
        );
        assert!(matches!(document.blocks[0], MarkdownBlock::Heading { level: 1, .. }));
        assert!(matches!(document.blocks[2], MarkdownBlock::List { .. }));
        match &document.blocks[4] {
            MarkdownBlock::FencedCode { info, source, closed, .. } => {
                assert_eq!(info, "markdown");
                assert!(source.contains("```mermaid"));
                assert!(*closed);
            }
            block => panic!("unexpected block: {block:?}"),
        }
    }

    #[test]
    fn flowchart_mermaid_golden() {
        let output = render(
            "```mermaid\nflowchart LR\nA[Start] -->|ok| B{Ready}\nB --> C((Ship))\n```",
            32,
        );
        assert_eq!(
            output.plain_text(),
            "┌─ mermaid · flowchart\n│ flowchart LR\n│ ┌─────────┐\n│ │A · Start│\n│ └─────────┘\n│ ◇ B · Ready ◇\n│ (C · Ship)\n│ edges\n│ A ─ok─▶ B\n│ B ───▶ C\n└─"
        );
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn invalid_mermaid_returns_visible_source_and_diagnostic() {
        let source = "sequenceDiagram\nAlice->>Bob: hello";
        let output = render(&format!("```mermaid\n{source}\n```"), 40);
        assert!(output.plain_text().contains("sequenceDiagram\n│ Alice->>Bob: hello"));
        assert!(output.plain_text().contains("Only flowchart/graph"));
        assert!(matches!(
            output.diagnostics.as_slice(),
            [RenderDiagnostic::Mermaid {
                diagnostic: MermaidDiagnostic {
                    kind: MermaidDiagnosticKind::UnsupportedDiagram,
                    ..
                },
                ..
            }]
        ));
    }

    #[test]
    fn mermaid_source_and_graph_limits_are_enforced() {
        let oversized = "x".repeat(MAX_MERMAID_SOURCE_BYTES + 1);
        let output = render(&format!("```mermaid\n{oversized}\n```"), 30);
        assert!(matches!(
            output.diagnostics.as_slice(),
            [RenderDiagnostic::Mermaid {
                diagnostic: MermaidDiagnostic {
                    kind: MermaidDiagnosticKind::OversizeSource,
                    ..
                },
                ..
            }]
        ));

        let output = render_markdown(
            "```mermaid\nflowchart TD\nA --> B\n```",
            &MarkdownRenderOptions {
                width: 30,
                mermaid: MermaidLimits {
                    max_nodes: 1,
                    ..MermaidLimits::default()
                },
                ..MarkdownRenderOptions::default()
            },
        );
        assert!(matches!(
            output.diagnostics.as_slice(),
            [RenderDiagnostic::Mermaid {
                diagnostic: MermaidDiagnostic {
                    kind: MermaidDiagnosticKind::OversizeGraph,
                    ..
                },
                ..
            }]
        ));
    }

    #[test]
    fn mermaid_rendering_is_deterministic() {
        let source = "```mermaid\nflowchart TD\nA[One] --> B[Two]\nA --> C[Three]\n```";
        let first = render(source, 24);
        for _ in 0..16 {
            assert_eq!(render(source, 24), first);
        }
    }

    #[test]
    fn compact_mermaid_edges_parse_without_spaces() {
        let chart = parse_mermaid("flowchart TD\nA-->B", MermaidLimits::default()).unwrap();
        assert_eq!(chart.nodes.iter().map(|node| node.id.as_str()).collect::<Vec<_>>(), vec!["A", "B"]);
        assert_eq!(chart.edges.len(), 1);
    }

    #[test]
    fn streaming_table_with_trailing_newline_stays_plain() {
        let output = render_markdown_streaming(
            "| a | b |\n| --- | --- |\n",
            &MarkdownRenderOptions {
                width: 20,
                ..MarkdownRenderOptions::default()
            },
        );
        assert_eq!(output.plain_text(), "| a | b |\n| --- | --- |");
    }

    #[test]
    fn partial_pipe_header_and_edge_rows_never_panic() {
        // Contract: exact panic input `|Language|Paradigm|` and related edge
        // rows must stay total under complete and streaming analysis.
        let options = MarkdownRenderOptions {
            width: 40,
            ..MarkdownRenderOptions::default()
        };
        let edges = [
            "|",
            "||",
            "|||",
            "|a|",
            "|a",
            "a|",
            "|Language|Paradigm|",
            "|Language|Paradigm|\n|",
            "|Language|Paradigm|\n||",
            "|Language|Paradigm|\n|---|",
            "| 名称 | 状态 |",
            "|a||b|\n| --- | --- | --- |\n|1||3|",
            "|\n| --- |\n| x |",
        ];
        for source in edges {
            let _ = render_markdown(source, &options);
            let _ = render_markdown_streaming(source, &options);
            let mut renderer = StreamingMarkdownRenderer::new(options.clone());
            renderer.push_str(source);
            let _ = renderer.output();
        }
    }

    #[test]
    fn streaming_table_chunk_boundary_upgrades_without_duplicate_prefix() {
        // Contract: header may arrive before separator/body across push_str
        // chunks; no panic, visible partial content, final table once without
        // duplicating the header prefix as plain text + table.
        let options = MarkdownRenderOptions {
            width: 40,
            ..MarkdownRenderOptions::default()
        };
        let mut renderer = StreamingMarkdownRenderer::new(options.clone());

        renderer.push_str("|Language|Paradigm|");
        let partial = renderer.output().plain_text();
        assert_eq!(partial, "|Language|Paradigm|");
        assert!(
            !partial.contains('┌'),
            "incomplete header must not layout as a table yet: {partial:?}"
        );

        renderer.push_str("\n| --- | --- |");
        let mid = renderer.output().plain_text();
        assert!(
            mid.contains("|Language|Paradigm|"),
            "header must remain visible before body arrives: {mid:?}"
        );
        assert!(
            !mid.contains('┌'),
            "header+separator without body stays plain in streaming: {mid:?}"
        );

        renderer.push_str("\n|Rust|multi-paradigm|");
        let final_text = renderer.output().plain_text();
        assert!(
            final_text.contains('┌') && final_text.contains("Language") && final_text.contains("Rust"),
            "completed table must render once: {final_text:?}"
        );
        assert_eq!(
            final_text.matches("|Language|Paradigm|").count(),
            0,
            "source header must not duplicate beside the laid-out table: {final_text:?}"
        );
        assert_eq!(
            final_text.matches('┌').count(),
            1,
            "table top border must appear exactly once: {final_text:?}"
        );

        let expected = render_markdown(
            "|Language|Paradigm|\n| --- | --- |\n|Rust|multi-paradigm|",
            &options,
        )
        .plain_text();
        assert_eq!(final_text, expected);

        // Char-boundary chunking of the same stream must match one-shot render.
        let full = "|Language|Paradigm|\n| --- | --- |\n|Rust|multi-paradigm|";
        let mut safe = StreamingMarkdownRenderer::new(options.clone());
        let mut offset = 0;
        while offset < full.len() {
            let mut end = (offset + 5).min(full.len());
            while end > offset && !full.is_char_boundary(end) {
                end -= 1;
            }
            if end == offset {
                end = full[offset..]
                    .chars()
                    .next()
                    .map(|ch| offset + ch.len_utf8())
                    .unwrap_or(full.len());
            }
            safe.push_str(&full[offset..end]);
            offset = end;
        }
        assert_eq!(safe.output().plain_text(), expected);
    }


    #[test]
    fn unclosed_mermaid_fence_stays_visible_during_streaming() {
        let output = render_markdown_streaming(
            "```mermaid\nflowchart TD\nA --> B",
            &MarkdownRenderOptions {
                width: 30,
                ..MarkdownRenderOptions::default()
            },
        );
        assert!(output.plain_text().contains("flowchart TD"));
        assert!(!output.plain_text().contains("mermaid · flowchart"));
        assert!(matches!(
            output.diagnostics.as_slice(),
            [RenderDiagnostic::UnclosedFence { source_line: 1 }]
        ));
    }

    #[test]
    fn code_fences_preserve_leading_indentation() {
        // Contract: fenced code is layout-verbatim; collapsing whitespace would
        // destroy Python/Makefile samples and hide indentation bugs.
        let output = render("```\n    indented\n\ttabbed\n  two\n```", 30);
        assert_eq!(
            output.plain_text(),
            "┌─ code\n    indented\n    tabbed\n  two\n└─"
        );
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn wrapped_table_rows_keep_header_body_and_border_roles() {
        // Contract: multi-line header/body cells must not steal TableBorder roles
        // from separators (index-based role bugs break TUI styling).
        let output = render(
            "| longheaderword | other |\n| --- | --- |\n| alphabeta gamma delta | x |",
            14,
        );
        let roles = output
            .lines
            .iter()
            .map(|line| line.role)
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            vec![
                LineRole::TableBorder,
                LineRole::TableHeader,
                LineRole::TableHeader,
                LineRole::TableHeader,
                LineRole::TableBorder,
                LineRole::TableBody,
                LineRole::TableBody,
                LineRole::TableBody,
                LineRole::TableBody,
                LineRole::TableBorder,
            ]
        );
        assert!(
            output
                .plain_lines()
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 14)
        );
    }

    #[test]
    fn render_width_is_clamped_to_one_thousand() {
        // Contract: pathological width must not allocate million-cell borders
        // (DoS via "─".repeat / column padding) on the render path.
        let break_output = render("---", 1_000_000);
        assert_eq!(break_output.lines.len(), 1);
        assert_eq!(
            UnicodeWidthStr::width(break_output.lines[0].text.as_str()),
            1_000
        );
        assert_eq!(break_output.lines[0].role, LineRole::ThematicBreak);

        let table = render("| a | b |\n| --- | --- |\n| 1 | 2 |", 1_000_000);
        assert!(table.diagnostics.is_empty());
        assert!(
            table
                .plain_lines()
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 1_000)
        );
        assert!(
            table
                .lines
                .iter()
                .map(|line| line.text.len())
                .max()
                .unwrap_or(0)
                <= 3_000
        );
    }

    #[test]
    fn multi_backtick_code_spans_keep_embedded_pipes() {
        // Contract: ``x|y`` and ```p|q``` are single cells; even-length tick
        // XOR bugs split cells and corrupt table geometry.
        let output = render(
            "| a | b |\n| --- | --- |\n| ``x|y`` | z |\n| ```p|q``` | r |",
            40,
        );
        assert_eq!(
            output.plain_text(),
            "┌──────────────────────┬───────────────┐\n│a                     │b              │\n├──────────────────────┼───────────────┤\n│``x|y``               │z              │\n│```p|q```             │r              │\n└──────────────────────┴───────────────┘"
        );
    }

    #[test]
    fn indic_conjuncts_and_zwj_family_respect_display_width() {
        // Contract: virama conjuncts and ZWJ emoji sequences must not overflow
        // the requested display width when they fit as a cluster.
        let ksha = render("क्ष", 2);
        assert_eq!(ksha.plain_text(), "क्ष");
        assert_eq!(UnicodeWidthStr::width(ksha.plain_text().as_str()), 2);

        let conj = render("क्\u{200d}ष", 2);
        assert_eq!(conj.plain_text(), "क्\u{200d}ष");
        assert_eq!(UnicodeWidthStr::width(conj.plain_text().as_str()), 2);

        let family = "👩\u{200d}👩\u{200d}👧\u{200d}👦";
        let table = render(
            &format!("| n | i |\n| --- | ---: |\n| z | {family} |"),
            16,
        );
        assert!(
            table
                .plain_lines()
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 16),
            "{:?}",
            table.plain_lines()
        );
        assert!(table.plain_text().contains(family));
    }

    #[test]
    fn mermaid_edge_and_output_cell_limits_are_enforced() {
        // Contract: complexity budgets must fail closed with source fallback,
        // not silently drop edges or allocate unbounded art.
        let edges = render_markdown(
            "```mermaid\nflowchart TD\nA --> B\n```",
            &MarkdownRenderOptions {
                width: 30,
                mermaid: MermaidLimits {
                    max_edges: 0,
                    ..MermaidLimits::default()
                },
                ..MarkdownRenderOptions::default()
            },
        );
        assert!(edges.plain_text().contains("source fallback"));
        assert!(edges.plain_text().contains("A --> B"));
        assert!(matches!(
            edges.diagnostics.as_slice(),
            [RenderDiagnostic::Mermaid {
                diagnostic: MermaidDiagnostic {
                    kind: MermaidDiagnosticKind::OversizeGraph,
                    ..
                },
                ..
            }]
        ));

        let cells = render_markdown(
            "```mermaid\nflowchart TD\nA[One] --> B[Two]\n```",
            &MarkdownRenderOptions {
                width: 40,
                mermaid: MermaidLimits {
                    max_output_cells: 1,
                    ..MermaidLimits::default()
                },
                ..MarkdownRenderOptions::default()
            },
        );
        assert!(cells.plain_text().contains("source fallback"));
        assert!(matches!(
            cells.diagnostics.as_slice(),
            [RenderDiagnostic::Mermaid {
                diagnostic: MermaidDiagnostic {
                    kind: MermaidDiagnosticKind::OutputLimit,
                    ..
                },
                ..
            }]
        ));
    }

    #[test]
    fn nested_markdown_fence_does_not_execute_inner_mermaid() {
        // Contract: mermaid inside an outer non-mermaid fence is inert source
        // (injection / nested-fence confusion).
        let output = render(
            "````markdown\n```mermaid\nflowchart TD\nA --> B\n```\n````",
            40,
        );
        assert!(output.plain_text().contains("┌─ code · markdown"));
        assert!(output.plain_text().contains("```mermaid"));
        assert!(output.plain_text().contains("A --> B"));
        assert!(!output.plain_text().contains("mermaid · flowchart"));
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn mermaid_info_aliases_and_lookalikes() {
        // Contract: mermaid-js is accepted; notmermaid must not be treated as
        // executable diagram chrome.
        let accepted = render("```mermaid-js\nflowchart TD\nA --> B\n```", 30);
        assert!(accepted.plain_text().contains("mermaid · flowchart"));
        assert!(accepted.plain_text().contains("A ───▶ B"));
        assert!(accepted.diagnostics.is_empty());
        assert_eq!(accepted.lines[0].role, LineRole::MermaidBorder);
        assert_eq!(accepted.lines[1].role, LineRole::MermaidNode);
        assert!(accepted
            .lines
            .iter()
            .any(|line| line.role == LineRole::MermaidEdge && line.text.contains('▶')));

        let rejected = render("```notmermaid\nflowchart TD\nA --> B\n```", 40);
        assert!(rejected.plain_text().contains("┌─ code · notmermaid"));
        assert!(!rejected.plain_text().contains("mermaid · flowchart"));
        assert!(rejected.diagnostics.is_empty());
    }

    #[test]
    fn empty_and_comment_only_mermaid_fall_back_visibly() {
        let empty = render("```mermaid\n```", 30);
        assert!(empty.plain_text().contains("source fallback"));
        assert!(matches!(
            empty.diagnostics.as_slice(),
            [RenderDiagnostic::Mermaid {
                diagnostic: MermaidDiagnostic {
                    kind: MermaidDiagnosticKind::InvalidSyntax,
                    message,
                    ..
                },
                ..
            }] if message.contains("empty")
        ));

        let comments = render("```mermaid\nflowchart TD\n%% only\n```", 30);
        assert!(comments.plain_text().contains("%% only"));
        assert!(matches!(
            comments.diagnostics.as_slice(),
            [RenderDiagnostic::Mermaid {
                diagnostic: MermaidDiagnostic {
                    kind: MermaidDiagnosticKind::InvalidSyntax,
                    message,
                    ..
                },
                ..
            }] if message.contains("no nodes")
        ));
    }

    #[test]
    fn mixed_cjk_table_and_mermaid_render_deterministically() {
        // Contract: HashMap node upsert + Unicode width must not introduce
        // order/width nondeterminism across repeated renders.
        let source = "| 名称 | 🚀 |\n| :---: | ---: |\n| 東京 | ok |\n\n```mermaid\nflowchart RL\nA[壹] -->|go| B((二))\n```";
        let first = render(source, 28);
        for _ in 0..16 {
            assert_eq!(render(source, 28), first);
        }
        assert!(first.diagnostics.is_empty());
        assert!(first.plain_text().contains("東京"));
        assert!(first.plain_text().contains("flowchart RL"));
        assert!(first.plain_text().contains("A ─go─▶ B"));
        assert!(
            first
                .plain_lines()
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 28)
        );
    }

    #[test]
    fn separator_column_mismatch_and_short_rows() {
        // Contract: bad separator arity stays plain text; short body rows pad
        // rather than panicking or dropping the table.
        let mismatched = render("| a | b | c |\n| --- | --- |\n| 1 | 2 | 3 |", 40);
        assert_eq!(
            mismatched.plain_text(),
            "| a | b | c |\n| --- | --- |\n| 1 | 2 | 3 |"
        );
        assert!(mismatched.diagnostics.is_empty());

        let short = render("| a | b |\n| --- | --- |\n| only |", 20);
        assert_eq!(
            short.plain_text(),
            "┌─────────┬────────┐\n│a        │b       │\n├─────────┼────────┤\n│only     │        │\n└─────────┴────────┘"
        );
    }

    #[test]
    fn graph_direction_shapes_and_undirected_edges() {
        // Contract: graph BT + rounded/rect shapes + --- edges render with
        // distinct chrome (not collapsed to flowchart TD arrows only).
        let output = render("```mermaid\ngraph BT\nA(Round) --- B[Rect]\n```", 40);
        assert_eq!(
            output.plain_text(),
            "┌─ mermaid · flowchart\n│ flowchart BU\n│ ╭─────────╮\n│ │A · Round│\n│ ╰─────────╯\n│ ┌────────┐\n│ │B · Rect│\n│ └────────┘\n│ edges\n│ A ──── B\n└─"
        );
        assert!(output.diagnostics.is_empty());
        assert!(output
            .lines
            .iter()
            .any(|line| line.role == LineRole::MermaidEdge && line.text.contains("────")));
        assert!(output
            .lines
            .iter()
            .take_while(|line| line.role != LineRole::MermaidEdge)
            .all(|line| matches!(
                line.role,
                LineRole::MermaidBorder | LineRole::MermaidNode
            )));
    }

    #[test]
    fn mismatched_and_shorter_fence_closers_stay_open() {
        // Contract: streaming/malformed fences must not close on the wrong
        // marker family or a shorter run.
        let mismatched = analyze_markdown("```js\ncode\n~~~");
        match &mismatched.blocks[0] {
            MarkdownBlock::FencedCode {
                closed,
                source,
                marker,
                ..
            } => {
                assert!(!*closed);
                assert_eq!(*marker, '`');
                assert!(source.contains("~~~"));
            }
            other => panic!("expected fence, got {other:?}"),
        }

        let streaming = render_markdown_streaming(
            "````md\n```\nbody",
            &MarkdownRenderOptions {
                width: 20,
                ..MarkdownRenderOptions::default()
            },
        );
        assert!(streaming.plain_text().contains("body"));
        assert!(matches!(
            streaming.diagnostics.as_slice(),
            [RenderDiagnostic::UnclosedFence { source_line: 1 }]
        ));
        assert!(!streaming.plain_text().contains("mermaid · flowchart"));
    }

    #[test]
    fn compact_mermaid_edge_renders_through_markdown_fence() {
        // Contract: A-->B without spaces is not only parseable but visible in
        // the public render path with correct edge role after the edges marker.
        let output = render("```mermaid\nflowchart TD\nA-->B\n```", 40);
        assert!(output.plain_text().contains("A ───▶ B"));
        assert!(output.diagnostics.is_empty());
        let edge_idx = output
            .lines
            .iter()
            .position(|line| line.text.contains("edges"))
            .expect("edges marker");
        assert_eq!(output.lines[edge_idx].role, LineRole::MermaidEdge);
        assert!(output.lines[edge_idx..]
            .iter()
            .filter(|line| line.text.contains('▶'))
            .all(|line| line.role == LineRole::MermaidEdge));
        assert!(output.lines[..edge_idx]
            .iter()
            .all(|line| matches!(
                line.role,
                LineRole::MermaidBorder | LineRole::MermaidNode
            )));
    }
    #[test]
    fn atx_closing_hashes_require_whitespace_separator() {
        let attached = analyze_markdown("# C#");
        assert!(matches!(
            &attached.blocks[0],
            MarkdownBlock::Heading { text, .. } if text == "C#"
        ));

        let closing = analyze_markdown("# Heading ###");
        assert!(matches!(
            &closing.blocks[0],
            MarkdownBlock::Heading { text, .. } if text == "Heading"
        ));
    }

    #[test]
    fn hard_wrap_is_lossless_for_uax29_graphemes() {
        let hindi = "नमस्ते";
        let output = render(hindi, 4);
        assert_eq!(output.plain_text().replace('\n', ""), hindi);
        assert!(!output.plain_text().contains('�'));

        let spacing_mark = "का";
        let spacing_output = render(spacing_mark, 1);
        assert_eq!(spacing_output.plain_text(), spacing_mark);

        let conjunct = "क्ष";
        let conjunct_output = render(conjunct, 1);
        assert_eq!(conjunct_output.plain_text(), conjunct);

        let family = "👩\u{200d}👩\u{200d}👧\u{200d}👦";
        let family_output = render(family, 1);
        assert_eq!(family_output.plain_text(), family);
    }

    #[test]
    fn emoji_heavy_mermaid_preflight_counts_terminal_cells() {
        let emoji = "😀";
        let label = emoji.repeat(400);
        let source = format!("flowchart TD\nA[{label}]");
        let art = render_mermaid_unicode(&source, 1_000, MermaidLimits::default())
            .expect("800-cell emoji label remains inside the output budget");
        assert_eq!(art.diagram.nodes[0].label, label);
    }

    #[test]
    fn plain_text_join_output_is_unchanged() {
        let output = MarkdownRenderOutput {
            lines: vec![
                NeutralLine {
                    text: "alpha".to_owned(),
                    role: LineRole::Text,
                },
                NeutralLine {
                    text: String::new(),
                    role: LineRole::Text,
                },
                NeutralLine {
                    text: "βeta".to_owned(),
                    role: LineRole::Text,
                },
            ],
            diagnostics: Vec::new(),
            truncated: false,
        };
        assert_eq!(output.plain_text(), "alpha\n\nβeta");
        assert_eq!(MarkdownRenderOutput::default().plain_text(), "");
    }

    #[test]
    fn streaming_renderer_freezes_completed_prefix() {
        let options = MarkdownRenderOptions {
            width: 40,
            ..MarkdownRenderOptions::default()
        };
        let mut renderer = StreamingMarkdownRenderer::new(options);
        renderer.push_str("# Frozen\n\nmutable");
        assert_eq!(renderer.frozen_source_bytes(), "# Frozen\n".len());
        let parsed_after_first = renderer.parsed_bytes();

        renderer.push_str(" tail");
        let reparsed = renderer.parsed_bytes() - parsed_after_first;
        assert_eq!(reparsed, "\nmutable tail".len());
        assert!(reparsed < "# Frozen\n\nmutable tail".len());
        assert_eq!(
            renderer.output(),
            render_markdown_streaming("# Frozen\n\nmutable tail", &options)
        );

        renderer.push_str("\n\nnext");
        assert_eq!(renderer.frozen_source_bytes(), "# Frozen\n\nmutable tail\n".len());
        assert_eq!(
            renderer.output(),
            render_markdown_streaming("# Frozen\n\nmutable tail\n\nnext", &options)
        );
    }
}
