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
        // Columns shrink to natural content width (Web parity) rather than
        // stretching to the full available width; alignment still applies.
        let output = render(
            "| left | middle | right |\n| :--- | :---: | ---: |\n| a | b | c |",
            28,
        );
        assert_eq!(
            output.plain_text(),
            "┌──────┬────────┬───────┐\n│ left │ middle │ right │\n├──────┼────────┼───────┤\n│ a    │   b    │     c │\n└──────┴────────┴───────┘"
        );
    }

    #[test]
    fn table_wide_unicode_and_emoji_use_display_width() {
        let output = render(
            "| Name | Stat |\n| --- | ---: |\n| Tokyo | ✅ |\n| rocket | 🚀 |",
            19,
        );
        for line in output.plain_lines() {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 19, "{line:?}");
        }
        assert_eq!(
            output.plain_text(),
            "┌────────┬──────┐\n│ Name   │ Stat │\n├────────┼──────┤\n│ Tokyo  │   ✅ │\n│ rocket │   🚀 │\n└────────┴──────┘"
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
            "┌─────┬─────┐\n│ ite │ det │\n│ m   │ ail │\n├─────┼─────┤\n│ alp │ one │\n│ ha  │ two │\n│     │ thr │\n│     │ ee  │\n└─────┴─────┘"
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
        assert_eq!(malformed.plain_text(), "| a | b |\n| -- | --- |\n| c | d |");

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
        assert!(matches!(
            document.blocks[0],
            MarkdownBlock::Heading { level: 1, .. }
        ));
        assert!(matches!(document.blocks[2], MarkdownBlock::List { .. }));
        match &document.blocks[4] {
            MarkdownBlock::FencedCode {
                info,
                source,
                closed,
                ..
            } => {
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
        // Canvas renderer: LR flowchart at width 32 exceeds the cell budget,
        // so it splits into two numbered panels with a boundary row for the
        // cross-panel edge. Arrows overwrite target borders (▶B), and edge
        // labels appear as bracketed glyphs on the canvas ([ok]).
        assert_eq!(
            output.plain_text(),
            "┌─ mermaid · flowchart [part 1/…\n│ flowchart LR\n│ ┌─────────┐\n│ │A · Start├─[ok]─▶ B · Ready ◇\n│ └─────────┘\n│ boundary: B → … C\n└─\n┌─ mermaid · flowchart [part 2/…\n│ (C · Ship)\n└─"
        );
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn sequence_diagram_mermaid_golden() {
        let output = render(
            "```mermaid\nsequenceDiagram\nparticipant A as Alice\nactor B as Bob\nA ->> B: hello\nB -->> A: ack\nA -) B: ping\nB --> A: pong\nA -x B: quit\nnote over A, B: working\nA => B: hmm\n```",
            48,
        );
        // Canvas renderer: sequence diagrams render as a topology canvas with
        // participant boxes, lifelines, and arrow lines connecting them. The
        // old table-like projection (│ Alice │──▶│ Bob : hello │) is gone.
        let text = output.plain_text();
        assert!(text.contains("┌─ mermaid · sequenceDiagram"), "{text}");
        assert!(text.contains("sequenceDiagram"), "{text}");
        assert!(text.contains("Alice"), "{text}");
        assert!(text.contains("Bob"), "{text}");
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("ack"), "{text}");
        assert!(text.contains("ping"), "{text}");
        assert!(text.contains("pong"), "{text}");
        assert!(text.contains("quit"), "{text}");
        assert!(text.contains("[note over A, B: working]"), "{text}");
        assert!(text.contains("A => B: hmm"), "{text}");
        assert!(text.contains('▶') && text.contains('◀'), "{text}");
        assert!(text.contains('○'), "{text}");
        assert!(text.contains('✕'), "{text}");
        assert!(
            !text.contains("│ Alice │──▶│"),
            "legacy sequence projection leaked: {text}"
        );
        // Single card, bounded width.
        assert_eq!(
            text.lines()
                .filter(|line| line.contains("┌─ mermaid ·"))
                .count(),
            1,
            "{text}"
        );
        assert_eq!(
            output
                .lines
                .iter()
                .filter(|line| line.role == LineRole::MermaidBorder && line.text == "└─")
                .count(),
            1,
            "{text}"
        );
        for line in &output.lines {
            assert!(
                UnicodeWidthStr::width(line.text.as_str()) <= 48,
                "line exceeds width 48: {:?}",
                line.text
            );
        }
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
    }

    #[test]
    fn sequence_diagram_streaming_matches_full_render() {
        let source = "```mermaid\nsequenceDiagram\nA ->> B: hello\nB -->> A: ack\n```";
        let full = render(source, 40);
        let streamed = render_markdown_streaming(
            source,
            &MarkdownRenderOptions {
                width: 40,
                ..MarkdownRenderOptions::default()
            },
        );
        assert_eq!(streamed.plain_text(), full.plain_text());
        assert!(
            streamed
                .plain_text()
                .contains("┌─ mermaid · sequenceDiagram")
        );
    }

    #[test]
    fn unsupported_mermaid_kind_returns_visible_source_and_diagnostic() {
        let source = "gantt\ntitle A Gantt Diagram";
        let output = render(&format!("```mermaid\n{source}\n```"), 40);
        assert!(output.plain_text().contains("mermaid · source fallback"));
        assert!(output.plain_text().contains("title A Gantt Diagram"));
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
        assert_eq!(
            chart
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B"]
        );
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
            "| Name | Stat |",
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
            final_text.contains('┌')
                && final_text.contains("Language")
                && final_text.contains("Rust"),
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
        // Contract: fenced code is layout-verbatim inside the frame; collapsing
        // whitespace would destroy Python/Makefile samples and hide indentation
        // bugs.
        let output = render("```\n    indented\n\ttabbed\n  two\n```", 30);
        let expected = format!(
            "╭── code ──{}╮\n│     indented{} │\n│     tabbed{} │\n│   two{} │\n╰{}╯",
            "─".repeat(18),
            " ".repeat(14),
            " ".repeat(16),
            " ".repeat(21),
            "─".repeat(28),
        );
        assert_eq!(output.plain_text(), expected);
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn wrapped_table_rows_keep_header_body_and_border_roles() {
        // Contract: content-aware wrapping emits one top border, one separator
        // border, and one bottom border. Every wrapped header row carries
        // TableHeader and every wrapped body row carries TableBody, so
        // index-based role bugs cannot steal TableBorder from separators. The
        // exact wrapped row count varies with content, so the assertion is
        // structural rather than a hardcoded count.
        let output = render(
            "| longheaderword | other |\n| --- | --- |\n| alphabeta gamma delta | x |",
            14,
        );
        let roles = output
            .lines
            .iter()
            .map(|line| line.role)
            .collect::<Vec<_>>();

        assert!(
            roles.len() >= 4,
            "table must render top/separator/bottom borders plus content: {roles:?}"
        );
        assert_eq!(
            roles.first(),
            Some(&LineRole::TableBorder),
            "top border: {roles:?}"
        );
        assert_eq!(
            roles.last(),
            Some(&LineRole::TableBorder),
            "bottom border: {roles:?}"
        );

        // The single separator border is the only TableBorder between the ends.
        let separator = roles[1..roles.len() - 1]
            .iter()
            .position(|role| *role == LineRole::TableBorder)
            .map(|index| index + 1)
            .expect("separator border between header and body");
        let header_roles = &roles[1..separator];
        let body_roles = &roles[separator + 1..roles.len() - 1];
        assert!(
            !header_roles.is_empty(),
            "header section must wrap content: {roles:?}"
        );
        assert!(
            header_roles
                .iter()
                .all(|role| *role == LineRole::TableHeader),
            "header section must be all TableHeader: {roles:?}"
        );
        assert!(
            !body_roles.is_empty(),
            "body section must wrap content: {roles:?}"
        );
        assert!(
            body_roles.iter().all(|role| *role == LineRole::TableBody),
            "body section must be all TableBody: {roles:?}"
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
            "┌─────┬───┐\n│ a   │ b │\n├─────┼───┤\n│ x|y │ z │\n│ p|q │ r │\n└─────┴───┘"
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
        let table = render(&format!("| n | i |\n| --- | ---: |\n| z | {family} |"), 16);
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
        // Contract: complexity budgets fail closed with source fallback rather
        // than silently dropping edges; an output-cell budget too small for a
        // single node panel also fails closed with an OutputLimit source
        // fallback (no silent split into sub-cell panels).
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
        // max_output_cells=1 is too small for even a single node panel, so
        // the flowchart falls back to the source with an OutputLimit
        // diagnostic rather than splitting into sub-cell panels.
        assert!(
            cells.plain_text().contains("source fallback"),
            "{}",
            cells.plain_text()
        );
        assert!(
            cells.plain_text().contains("A[One] --> B[Two]"),
            "{}",
            cells.plain_text()
        );
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
        assert!(output.plain_text().contains("╭── code · markdown"));
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
        // Canvas renderer: TD (vertical) arrows are ▼, not ▶; the canvas has
        // no literal "edges" section — every non-border line is MermaidNode.
        assert!(
            accepted.plain_text().contains("A · A"),
            "{:?}",
            accepted.plain_text()
        );
        assert!(
            accepted.plain_text().contains("B · B"),
            "{:?}",
            accepted.plain_text()
        );
        assert!(
            accepted.plain_text().contains('▼'),
            "{:?}",
            accepted.plain_text()
        );
        assert!(
            !accepted
                .plain_text()
                .lines()
                .any(|line| line.trim() == "│ edges")
        );
        assert!(accepted.diagnostics.is_empty());
        assert_eq!(accepted.lines[0].role, LineRole::MermaidBorder);
        assert_eq!(accepted.lines[1].role, LineRole::MermaidNode);
        // All non-border lines carry MermaidNode (canvas renderer).
        assert!(
            accepted
                .lines
                .iter()
                .filter(|line| line.role == LineRole::MermaidNode)
                .any(|line| line.text.contains('▼'))
        );

        let rejected = render("```notmermaid\nflowchart TD\nA --> B\n```", 40);
        assert!(rejected.plain_text().contains("╭── code · notmermaid"));
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
    fn mixed_wide_unicode_table_and_mermaid_render_deterministically() {
        // Contract: HashMap node upsert + Unicode width must not introduce
        // order/width nondeterminism across repeated renders.
        let source = "| Name | 🚀 |\n| :---: | ---: |\n| Tokyo | ok |\n\n```mermaid\nflowchart RL\nA[One] -->|go| B((Two))\n```";
        let first = render(source, 28);
        for _ in 0..16 {
            assert_eq!(render(source, 28), first);
        }
        assert!(first.diagnostics.is_empty());
        assert!(first.plain_text().contains("Tokyo"));
        assert!(first.plain_text().contains("flowchart RL"));
        // Canvas renderer: RL (horizontal, right-to-left) arrows are ◀, and
        // edge labels appear as bracketed glyphs ([go]) on the canvas.
        assert!(
            first.plain_text().contains("[go]"),
            "{:?}",
            first.plain_text()
        );
        assert!(first.plain_text().contains('◀'), "{:?}", first.plain_text());
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
            "┌──────┬───┐\n│ a    │ b │\n├──────┼───┤\n│ only │   │\n└──────┴───┘"
        );
    }

    #[test]
    fn graph_direction_shapes_and_undirected_edges() {
        // Contract: graph BT + rounded/rect shapes + --- edges render with
        // distinct chrome (not collapsed to flowchart TD arrows only).
        let output = render("```mermaid\ngraph BT\nA(Round) --- B[Rect]\n```", 40);
        // Canvas renderer: BU (bottom-up) vertical layout places B on top and
        // A on bottom, with the undirected edge (---, no arrow) drawn as a
        // vertical line using ┴ (source connector) and ┼ (no-arrow target).
        // No literal "edges" section — every non-border line is MermaidNode.
        assert_eq!(
            output.plain_text(),
            "┌─ mermaid · flowchart\n│ flowchart BU\n│ ┌────────┐\n│ │B · Rect│\n│ └────┼───┘\n│      │\n│      │\n│      │\n│      │\n│ ╭────┴────╮\n│ │A · Round│\n│ ╰─────────╯\n└─"
        );
        assert!(output.diagnostics.is_empty());
        // Undirected edge: ┼ (no-arrow) at target, ┴ at source, │ for the line.
        assert!(
            output.plain_text().contains('┼'),
            "{:?}",
            output.plain_text()
        );
        assert!(
            output.plain_text().contains('┴'),
            "{:?}",
            output.plain_text()
        );
        // No literal "edges" section in the canvas renderer.
        assert!(
            !output
                .plain_text()
                .lines()
                .any(|line| line.trim() == "│ edges")
        );
        // All non-border lines carry MermaidNode (canvas renderer).
        assert!(
            output
                .lines
                .iter()
                .filter(|line| line.role == LineRole::MermaidNode)
                .all(|line| !line.text.trim().starts_with("edges"))
        );
        assert!(
            output
                .lines
                .iter()
                .all(|line| matches!(line.role, LineRole::MermaidBorder | LineRole::MermaidNode))
        );
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
        // Contract: A-->B without spaces is parseable and visible in the
        // public render path. The canvas renderer draws TD (vertical) edges as
        // ▼ glyphs on the canvas — there is no literal "edges" section, and
        // every non-border line carries MermaidNode.
        let output = render("```mermaid\nflowchart TD\nA-->B\n```", 40);
        assert!(
            output.plain_text().contains("A · A"),
            "{:?}",
            output.plain_text()
        );
        assert!(
            output.plain_text().contains("B · B"),
            "{:?}",
            output.plain_text()
        );
        assert!(
            output.plain_text().contains('▼'),
            "{:?}",
            output.plain_text()
        );
        assert!(
            !output
                .plain_text()
                .lines()
                .any(|line| line.trim() == "│ edges")
        );
        assert!(output.diagnostics.is_empty());
        // All non-border lines carry MermaidNode (canvas renderer).
        assert!(
            output
                .lines
                .iter()
                .all(|line| matches!(line.role, LineRole::MermaidBorder | LineRole::MermaidNode))
        );
        // The vertical arrow glyph ▼ appears in a MermaidNode line.
        assert!(
            output
                .lines
                .iter()
                .filter(|line| line.role == LineRole::MermaidNode)
                .any(|line| line.text.contains('▼'))
        );
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
    fn reported_class_diagram_renders_single_class_card() {
        // Exact user source: class members + Application --> Session and
        // Agent ..> AgentTool : via context. One successful classDiagram card.
        let source = "```mermaid\n\
classDiagram\n\
class Application {\n\
+run()\n\
}\n\
class Session {\n\
+id: String\n\
}\n\
class Agent {\n\
+tools: Vec\n\
}\n\
class AgentTool {\n\
+name: String\n\
}\n\
Application --> Session\n\
Agent ..> AgentTool : via context\n\
```";
        let output = render(source, 48);
        let text = output.plain_text();
        assert_eq!(
            text.lines()
                .filter(|line| line.contains("┌─ mermaid ·"))
                .count(),
            1,
            "{text}"
        );
        assert!(text.contains("┌─ mermaid · classDiagram"), "{text}");
        assert!(!text.contains("┌─ mermaid · flowchart"), "{text}");
        assert!(!text.contains("source fallback"), "{text}");
        assert!(!text.contains("! mermaid:"), "{text}");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert!(text.contains("+run()"), "{text}");
        assert!(text.contains("+id: String"), "{text}");
        assert!(text.contains("+tools: Vec"), "{text}");
        assert!(text.contains("+name: String"), "{text}");
        assert!(text.contains("Application ───▶ Session"), "{text}");
        assert!(text.contains("Agent ··via context··▶ AgentTool"), "{text}");
        assert_eq!(
            output
                .lines
                .iter()
                .filter(|line| {
                    line.role == LineRole::MermaidBorder && line.text.starts_with("└─")
                })
                .count(),
            1,
            "{text}"
        );
    }

    #[test]
    fn reported_labeled_subgraph_flowchart_renders_split_or_single_card() {
        // Exact user source: flowchart LR + subgraph records["SessionRecord types"].
        // The canvas renderer may split into numbered panels when the rank
        // exceeds the cell budget; cross-panel edges appear as boundary rows.
        let source = "```mermaid\n\
flowchart LR\n\
subgraph records[\"SessionRecord types\"]\n\
A[Session] --> B[Message]\n\
B --> C[ToolCall]\n\
end\n\
X[User] --> A\n\
```";
        let output = render(source, 48);
        let text = output.plain_text();
        // Successful render (no source fallback, no diagnostic).
        assert!(text.contains("┌─ mermaid · flowchart"), "{text}");
        assert!(!text.contains("source fallback"), "{text}");
        assert!(!text.contains("! mermaid:"), "{text}");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        // Every panel closes its own frame: card count equals footer count.
        let card_count = text
            .lines()
            .filter(|line| line.contains("┌─ mermaid ·"))
            .count();
        let footer_count = output
            .lines
            .iter()
            .filter(|line| line.role == LineRole::MermaidBorder && line.text.starts_with("└─"))
            .count();
        assert!(card_count >= 1, "{text}");
        assert_eq!(card_count, footer_count, "{text}");
        // Subgraph and node labels survive the split.
        assert!(
            text.contains("subgraph records · SessionRecord types"),
            "{text}"
        );
        assert!(text.contains("end subgraph records"), "{text}");
        assert!(text.contains("A · Session"), "{text}");
        assert!(text.contains("B · Message"), "{text}");
        assert!(text.contains("C · ToolCall"), "{text}");
        assert!(text.contains("X · User"), "{text}");
        // Canvas renderer: edges are attached glyphs, not literal edge lines.
        // Horizontal arrows (▶) overwrite target borders; cross-panel edges
        // appear as boundary rows. No literal "edges" section.
        assert!(!text.lines().any(|line| line.trim() == "│ edges"), "{text}");
        // Every edge stays visible: either as an attached ▶ glyph or a
        // boundary row.
        let arrow_count = text.matches('▶').count();
        let boundary_count = text
            .lines()
            .filter(|line| line.contains("boundary:"))
            .count();
        assert_eq!(
            arrow_count + boundary_count,
            3,
            "all 3 edges must stay visible: {text}"
        );
        // All non-border lines carry MermaidNode (canvas renderer).
        assert!(
            output
                .lines
                .iter()
                .all(|line| matches!(line.role, LineRole::MermaidBorder | LineRole::MermaidNode))
        );
    }

    #[test]
    fn reported_class_and_flow_cards_have_truthful_titles() {
        let class = parse_mermaid(
            "classDiagram\nclass Application {\n+run()\n}\nclass Session {\n+id: String\n}\nApplication --> Session\nAgent ..> AgentTool : via context\n",
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(class.nodes.len(), 4);
        assert_eq!(class.edges.len(), 2);

        let flow = parse_mermaid(
            "flowchart LR\nsubgraph records[\"SessionRecord types\"]\nA[Session] --> B[Message]\nB --> C[ToolCall]\nend\nX[User] --> A\n",
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(flow.direction, FlowDirection::LeftRight);
        assert_eq!(flow.nodes.len(), 4);
        assert_eq!(flow.edges.len(), 3);

        let art_class = render_mermaid_unicode(
            "classDiagram\nclass Application {\n+run()\n}\nclass Session {\n+id: String\n}\nclass Agent {\n+tools: Vec\n}\nclass AgentTool {\n+name: String\n}\nApplication --> Session\nAgent ..> AgentTool : via context\n",
            48,
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(art_class.kind, MermaidDiagramKind::ClassDiagram);

        let art_flow = render_mermaid_unicode(
            "flowchart LR\nsubgraph records[\"SessionRecord types\"]\nA[Session] --> B[Message]\nB --> C[ToolCall]\nend\nX[User] --> A\n",
            48,
            MermaidLimits::default(),
        )
        .unwrap();
        assert_eq!(art_flow.kind, MermaidDiagramKind::Flowchart);
    }

    #[test]
    fn plain_text_join_output_is_unchanged() {
        let output = MarkdownRenderOutput {
            lines: vec![
                NeutralLine {
                    text: "alpha".to_owned(),
                    role: LineRole::Text,
                    inline_styles: Vec::new(),
                    language: None,
                },
                NeutralLine {
                    text: String::new(),
                    role: LineRole::Text,
                    inline_styles: Vec::new(),
                    language: None,
                },
                NeutralLine {
                    text: "βeta".to_owned(),
                    role: LineRole::Text,
                    inline_styles: Vec::new(),
                    language: None,
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
        assert_eq!(
            renderer.frozen_source_bytes(),
            "# Frozen\n\nmutable tail\n".len()
        );
        assert_eq!(
            renderer.output(),
            render_markdown_streaming("# Frozen\n\nmutable tail\n\nnext", &options)
        );
    }

    /// Box-drawing / grid chrome that only ever legitimately appears inside a
    /// laid-out table, code fence, or mermaid frame. Ordinary prose lines must
    /// never carry any of these glyphs (prose is never routed through a
    /// table/grid projection).
    const GRID_CHROME: &[char] = &[
        '┌', '┬', '┐', '├', '┼', '┤', '│', '└', '┴', '┘', '─', '╭', '╮', '╰', '╯', '╎', '╏', '│',
    ];

    /// Assert a rendered line carries no interior run of two or more spaces
    /// after its leading indentation (the only multi-space run a bullet
    /// continuation legitimately begins with). This catches soft-wrap
    /// justification / column padding expanding single word separators into
    /// "huge inter-word spacing".
    fn assert_no_expanded_spaces(line: &str) {
        let stripped = line.trim_start();
        assert!(
            !stripped.contains("  "),
            "prose must never expand interior spaces: {line:?}"
        );
    }

    #[test]
    fn short_intro_bullets_keep_continuation_without_expanded_spaces_and_never_layout_as_table() {
        // Exact semantic fixture: a short intro heading, then bullets mixing
        // Chinese and English phrases, a long git hash, and repeated
        // commit/apply-path tokens. Reported malformed render spreads the
        // phrase fragments into widely spaced text and ends with a line of
        // huge inter-word spacing.
        let source = "intro\n\
- recap apply Proposal backup bytes + 5 dual-proposer remint + sub\n\
- commit 9f3a2b1c7d4e5f6a8b9c0d1e2f3a4b5c6d7e8f9a apply path\n\
- repeated verify fingerprint cert rm /tmp/x.bin ready\n\
\n\
最后一行包含中文和 English words mixed together 非常长的一句话 that wraps with spaces.";
        // Ordinary prose has no pipe separator, so it must never produce a
        // table block — neither complete nor streaming analysis.
        let document = analyze_markdown(source);
        assert!(
            document
                .blocks
                .iter()
                .all(|block| !matches!(block, MarkdownBlock::Table(_))),
            "prose must never be routed through table layout: {document:#?}"
        );

        for width in [16usize, 24, 40, 60] {
            let output = render(source, width);
            for line in &output.lines {
                let text = line.text.as_str();
                assert!(
                    UnicodeWidthStr::width(text) <= width,
                    "line exceeds width {width}: {text:?}"
                );
                match line.role {
                    LineRole::Text | LineRole::ListMarker => {
                        // Prose/list text: single-space separators only.
                        assert_no_expanded_spaces(text);
                        // No prose line may carry table/grid/fence chrome.
                        assert!(
                            !text.chars().any(|c| GRID_CHROME.contains(&c)),
                            "prose routed through grid chrome at width {width}: {text:?}"
                        );
                    }
                    _ => {}
                }
            }
            // Bullet continuation rows must be indented (not detached into
            // stray fragments) and must not repeat the bullet marker.
            let lines = output.plain_lines();
            for pair in lines.windows(2) {
                let continuation = pair[1].as_str();
                if continuation.starts_with(' ') && !continuation.trim().is_empty() {
                    assert!(
                        !continuation.trim_start().starts_with('•'),
                        "continuation must not carry a fresh bullet marker: {continuation:?}"
                    );
                    assert_no_expanded_spaces(continuation);
                }
            }
        }
    }

    #[test]
    fn narrow_width_cjk_rust_table_code_fixture_is_coherent_and_bounded() {
        let source = "# 测试标题\n\
\n\
这里是一段很长的中文文本，没有任何空格分割，用来验证在较窄宽度下 CJK 字符能否正确换行而不产生碎片化断裂 token。\n\
\n\
- 第一项 包含 mix English words 和中文\n\
- 第二项 继续测试 12345 67890\n\
\n\
```rust\n\
fn main() {\n\
    let x = \"hello\";\n\
    println!(\"{}\", x);\n\
}\n\
```\n\
\n\
| 列一 | 列二 |\n\
| --- | --- |\n\
| 内容甲 | 内容乙 |\n\
| alpha | beta gamma |";

        for width in [24usize, 32, 48, 80] {
            let output = render(source, width);
            let lines = &output.lines;
            // Coherent and bounded: every row stays within the render width.
            for line in lines {
                assert!(
                    UnicodeWidthStr::width(line.text.as_str()) <= width,
                    "row exceeds width {width}: {:?}",
                    line.text
                );
            }
            // Prose (paragraph + CJK) never carries interior double spaces and
            // never carries grid chrome.
            for line in lines {
                match line.role {
                    LineRole::Text | LineRole::ListMarker => {
                        assert_no_expanded_spaces(&line.text);
                        assert!(
                            !line.text.chars().any(|c| GRID_CHROME.contains(&c)),
                            "prose routed through grid chrome: {:?}",
                            line.text
                        );
                    }
                    _ => {}
                }
            }
            // Code fence stays a single coherent frame: one top `╭`, one
            // bottom `╰`, every body row width == frame width with side bars.
            let code_fence_rows: Vec<&str> = lines
                .iter()
                .filter(|l| l.role == LineRole::CodeFence || l.role == LineRole::Code)
                .map(|l| l.text.as_str())
                .collect();
            assert_eq!(
                code_fence_rows
                    .iter()
                    .filter(|l| l.starts_with('╭'))
                    .count(),
                1,
                "code top border must appear exactly once: {code_fence_rows:?}"
            );
            assert_eq!(
                code_fence_rows
                    .iter()
                    .filter(|l| l.starts_with('╰'))
                    .count(),
                1,
                "code bottom border must appear exactly once: {code_fence_rows:?}"
            );
            let frame_width = UnicodeWidthStr::width(code_fence_rows[0]);
            for row in &code_fence_rows {
                assert_eq!(
                    UnicodeWidthStr::width(*row),
                    frame_width,
                    "all code body rows share one frame width: {row:?}"
                );
            }
            for row in code_fence_rows.iter().filter(|l| l.starts_with('│')) {
                assert!(
                    row.ends_with('│') && UnicodeWidthStr::width(*row) == frame_width,
                    "code body row must be side-bordered and frame-width: {row:?}"
                );
            }
            // Table stays a single coherent frame: one `┌`, one `└`, one
            // header separator, all rows share one width.
            let table_rows: Vec<&str> = lines
                .iter()
                .filter(|l| {
                    matches!(
                        l.role,
                        LineRole::TableBorder | LineRole::TableHeader | LineRole::TableBody
                    )
                })
                .map(|l| l.text.as_str())
                .collect();
            assert!(!table_rows.is_empty(), "table must render at width {width}");
            assert_eq!(
                table_rows.iter().filter(|l| l.starts_with('┌')).count(),
                1,
                "table top border exactly once: {table_rows:?}"
            );
            assert_eq!(
                table_rows.iter().filter(|l| l.starts_with('└')).count(),
                1,
                "table bottom border exactly once: {table_rows:?}"
            );
            assert!(
                table_rows
                    .iter()
                    .any(|l| l.starts_with('├') && l.contains('┼')),
                "header separator must render: {table_rows:?}"
            );
            let table_width = UnicodeWidthStr::width(table_rows[0]);
            assert!(
                table_width <= width,
                "table width bounded: {table_width} > {width}"
            );
            for row in &table_rows {
                assert_eq!(
                    UnicodeWidthStr::width(*row),
                    table_width,
                    "all table rows share one frame width: {row:?}"
                );
            }
            // All CJK characters survive: the paragraph source, re-wrapped, is
            // preserved verbatim (no dropped or reordered glyphs).
            let paragraph_source = "这里是一段很长的中文文本，没有任何空格分割，用来验证在较窄宽度下 CJK 字符能否正确换行而不产生碎片化断裂 token。";
            let paragraph_glyphs: String = lines
                .iter()
                .filter(|l| l.role == LineRole::Text)
                .map(|l| l.text.trim().to_owned())
                .collect::<Vec<_>>()
                .join("");
            for character in paragraph_source.chars().filter(|c| !c.is_whitespace()) {
                assert!(
                    paragraph_glyphs.contains(character),
                    "CJK/ASCII glyph lost in wrapping at width {width}: {character:?}"
                );
            }
        }
    }

    #[test]
    fn chunked_streaming_final_equals_settled_render() {
        let source = "# 测试标题\n\
这里是一段很长的中文文本，没有任何空格分割，用来验证在较窄宽度下 CJK 字符能否正确换行而不产生碎片化断裂 token。\n\
\n\
- recap apply Proposal backup bytes + 5 dual-proposer remint + sub\n\
- commit 9f3a2b1c7d4e5f6a8b9c0d1e2f3a4b5c6d7e8f9a apply path\n\
\n\
```rust\n\
fn main() {\n\
    let x = \"hello\";\n\
    println!(\"{}\", x);\n\
}\n\
```\n\
| 列一 | 列二 |\n\
| --- | --- |\n\
| 内容甲 | 内容乙 |\n\
| alpha | beta gamma |";

        for width in [40usize, 60, 80] {
            let options = MarkdownRenderOptions {
                width,
                ..MarkdownRenderOptions::default()
            };
            let expected = render_markdown(source, &options).plain_text();
            let margins: Vec<usize> = source
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(source.len()))
                .collect();
            for step in [1usize, 3, 5, 11, 23] {
                let mut renderer = StreamingMarkdownRenderer::new(options);
                for idx in (0..margins.len().saturating_sub(1)).step_by(step) {
                    let start = margins[idx];
                    let end = margins[(idx + step).min(margins.len() - 1)];
                    renderer.push_str(&source[start..end]);
                }
                assert_eq!(
                    renderer.output().plain_text(),
                    expected,
                    "chunked streaming final must equal settled render at width {width}, step {step}"
                );
            }
        }
    }
}
