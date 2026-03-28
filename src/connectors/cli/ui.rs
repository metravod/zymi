use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::app::{AddModelStep, App, ChatEntry, LeftPanelSection, PROVIDER_OPTIONS};
use super::markdown::render_markdown;
use super::theme;

pub fn draw(f: &mut Frame, app: &mut App) {
    let terminal_width = f.area().width;

    // Auto-collapse panels on narrow terminals
    let show_left = app.left_panel_visible && terminal_width >= 100;
    let show_right = app.right_panel_visible && terminal_width >= 100;

    let left_width: u16 = if show_left { 28 } else { 0 };
    let right_width: u16 = if show_right { 36 } else { 0 };

    // Build horizontal constraints
    let mut h_constraints = Vec::new();
    if show_left {
        h_constraints.push(Constraint::Length(left_width));
    }
    h_constraints.push(Constraint::Min(40)); // center
    if show_right {
        h_constraints.push(Constraint::Length(right_width));
    }

    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(h_constraints)
        .split(f.area());

    let center_idx = if show_left { 1 } else { 0 };

    // Left panel
    if show_left {
        draw_left_panel(f, app, h_chunks[0]);
    }

    // Center column
    draw_center_column(f, app, h_chunks[center_idx]);

    // Right panel
    if show_right {
        let right_idx = h_chunks.len() - 1;
        draw_right_panel(f, app, h_chunks[right_idx]);
    }

    // Overlays (rendered on top of everything)
    if app.model_selector_open {
        draw_model_selector(f, app);
    }

    if app.add_model_form.is_some() {
        draw_add_model_form(f, app);
    }
}

fn draw_center_column(f: &mut Frame, app: &mut App, area: Rect) {
    let input_lines = app.input.lines().len().max(1) as u16;
    let max_input_height = (area.height / 3).max(3);
    let input_height = (input_lines + 2).clamp(3, max_input_height);

    let constraints = vec![
        Constraint::Length(6),  // header
        Constraint::Min(5),    // chat area
        Constraint::Length(1), // status line
        Constraint::Length(input_height), // input
        Constraint::Length(1), // hint bar
    ];

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_header(f, chunks[0], &app.current_model_id, app.copy_mode);
    draw_chat(f, app, chunks[1]);
    draw_status_line(f, app, chunks[2]);
    draw_input(f, app, chunks[3]);
    draw_hint_bar(f, app, chunks[4]);
}

fn draw_header(f: &mut Frame, area: Rect, model: &str, copy_mode: bool) {
    let art = Style::default()
        .fg(theme::ACCENT)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme::SUBTEXT);
    let bold = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);

    let pad = "  ";

    // Line 1: top of slant art
    let line1 = Line::from(vec![
        Span::styled("     ____  __  __  ____ ___   __", art),
    ]);

    // Line 2: middle + version
    let line2 = Line::from(vec![
        Span::styled("    /_  / / / / / / __ `__ \\ / /", art),
        Span::raw(pad),
        Span::styled(format!("v{}", env!("CARGO_PKG_VERSION")), dim),
    ]);

    // Line 3: middle + model + copy mode
    let mut line3_spans = vec![
        Span::styled("     / /_/ /_/ / / / / / / // /", art),
        Span::raw(pad),
        Span::styled("model: ", dim),
        Span::styled(model.to_string(), bold),
        Span::styled("  [Ctrl+M]", dim),
    ];

    if copy_mode {
        line3_spans.push(Span::raw("  "));
        line3_spans.push(Span::styled(
            " COPY ",
            Style::default()
                .fg(theme::SURFACE)
                .bg(theme::WARNING)
                .add_modifier(Modifier::BOLD),
        ));
        line3_spans.push(Span::styled(" Ctrl+Y", dim));
    } else {
        line3_spans.push(Span::styled("  copy: off", dim));
        line3_spans.push(Span::styled("  [Ctrl+Y]", dim));
    }

    let line3 = Line::from(line3_spans);

    // Line 4: bottom + cwd
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let line4 = Line::from(vec![
        Span::styled("    /___/\\__,_/ /_/ /_/ /_//_/", art),
        Span::raw(pad),
        Span::styled(cwd, dim),
    ]);

    // Line 5: empty spacer
    let line5 = Line::default();

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(theme::BORDER));

    let header_widget = Paragraph::new(vec![line1, line2, line3, line4, line5])
        .block(block)
        .style(Style::default().bg(theme::SURFACE));

    f.render_widget(header_widget, area);
}

fn draw_chat(f: &mut Frame, app: &mut App, area: Rect) {
    let mut all_lines: Vec<Line<'static>> = Vec::new();

    for entry in &app.messages {
        match entry {
            ChatEntry::UserMessage(text) => {
                all_lines.push(Line::default());
                all_lines.push(Line::from(vec![
                    Span::styled(
                        " > ",
                        Style::default()
                            .fg(theme::SUCCESS)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        text.clone(),
                        Style::default()
                            .fg(theme::TEXT)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                all_lines.push(Line::default());
            }
            ChatEntry::AssistantChunk {
                content,
                is_complete,
            } => {
                let md_lines = render_markdown(content);
                for line in md_lines {
                    let mut prefixed = vec![Span::raw("   ")];
                    prefixed.extend(line.spans);
                    all_lines.push(Line::from(prefixed));
                }
                if !is_complete {
                    all_lines.push(Line::from(Span::styled(
                        format!("   {} ", app.spinner()),
                        Style::default().fg(theme::ACCENT),
                    )));
                }
                all_lines.push(Line::default());
            }
            ChatEntry::ToolCall {
                name,
                arguments,
                result,
                is_error,
                is_running,
            } => {
                let status_icon = if *is_running {
                    Span::styled(
                        format!("{} ", app.spinner()),
                        Style::default().fg(theme::TOOL),
                    )
                } else if *is_error {
                    Span::styled("✗ ", Style::default().fg(theme::ERROR))
                } else {
                    Span::styled("✓ ", Style::default().fg(theme::SUCCESS))
                };

                all_lines.push(Line::from(vec![
                    Span::raw("   "),
                    status_icon,
                    Span::styled(
                        name.clone(),
                        Style::default()
                            .fg(theme::TOOL)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));

                // Show arguments (truncated)
                let args_display = truncate_str(arguments, 120);
                if !args_display.is_empty() {
                    all_lines.push(Line::from(vec![
                        Span::raw("     "),
                        Span::styled(args_display, Style::default().fg(theme::SUBTEXT)),
                    ]));
                }

                // Show result if available
                if let Some(result_text) = result {
                    let result_style = if *is_error {
                        Style::default().fg(theme::ERROR)
                    } else {
                        Style::default().fg(theme::SUCCESS)
                    };

                    let result_display = truncate_str(result_text, 500);
                    for result_line in result_display.lines() {
                        all_lines.push(Line::from(vec![
                            Span::raw("     "),
                            Span::styled(result_line.to_string(), result_style),
                        ]));
                    }
                }

                all_lines.push(Line::default());
            }
            ChatEntry::SystemMessage(text) => {
                all_lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(text.clone(), Style::default().fg(theme::WARNING)),
                ]));
                all_lines.push(Line::default());
            }
            ChatEntry::DebugMessage { caller, content } => {
                let debug_style = Style::default()
                    .fg(theme::DEBUG)
                    .add_modifier(Modifier::DIM);
                let label_style = Style::default()
                    .fg(theme::DEBUG)
                    .add_modifier(Modifier::BOLD);

                all_lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(format!("[DEBUG: {caller}] "), label_style),
                ]));
                let display = truncate_str(content, 500);
                for line in display.lines() {
                    all_lines.push(Line::from(vec![
                        Span::raw("     "),
                        Span::styled(line.to_string(), debug_style),
                    ]));
                }
                all_lines.push(Line::default());
            }
        }
    }

    // Show "Thinking..." indicator when processing and no active stream/tool
    if app.is_processing {
        let show_thinking = match app.messages.last() {
            Some(ChatEntry::AssistantChunk { is_complete, .. }) => *is_complete,
            Some(ChatEntry::ToolCall { is_running, .. }) => !*is_running,
            _ => true,
        };

        if show_thinking {
            all_lines.push(Line::from(Span::styled(
                format!("   {} Thinking...", app.spinner()),
                Style::default().fg(theme::ACCENT),
            )));
            all_lines.push(Line::default());
        }
    }

    // Inline approval request
    if let Some(ref pending) = app.pending_approval {
        let warning_style = Style::default()
            .fg(theme::WARNING)
            .add_modifier(Modifier::BOLD);
        let text_style = Style::default().fg(theme::TEXT);
        let dim_style = Style::default().fg(theme::SUBTEXT);

        all_lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled("⚠ Approve?", warning_style),
        ]));

        // Full tool description (strip HTML tags from Telegram compat)
        let raw_desc = pending
            .tool_description
            .replace("<code>", "")
            .replace("</code>", "")
            .replace("<br>", "\n");

        for line in raw_desc.lines() {
            all_lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(line.to_string(), text_style),
            ]));
        }

        // LLM explanation
        if let Some(ref expl) = pending.explanation {
            all_lines.push(Line::default());
            for line in expl.lines() {
                all_lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(line.to_string(), dim_style),
                ]));
            }
        }

        // Yes/No selector with arrow key selection
        all_lines.push(Line::default());
        let (yes_style, no_style) = if app.approval_selected {
            (
                Style::default()
                    .fg(theme::SURFACE)
                    .bg(theme::SUCCESS)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme::SUBTEXT),
            )
        } else {
            (
                Style::default().fg(theme::SUBTEXT),
                Style::default()
                    .fg(theme::SURFACE)
                    .bg(theme::ERROR)
                    .add_modifier(Modifier::BOLD),
            )
        };

        all_lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(" Yes ", yes_style),
            Span::raw("  "),
            Span::styled(" No ", no_style),
            Span::raw("  "),
            Span::styled("← → select  Enter confirm", dim_style),
        ]));
        all_lines.push(Line::default());
    }

    // Add bottom padding so the last message doesn't stick to the viewport edge
    let bottom_padding = (area.height / 2).max(2);
    for _ in 0..bottom_padding {
        all_lines.push(Line::default());
    }

    let chat = Paragraph::new(all_lines)
        .wrap(Wrap { trim: false });

    // Use line_count to get the actual wrapped height (accounts for line wrapping)
    let total_height = chat.line_count(area.width) as u16;
    let inner_height = area.height;
    app.total_content_height = total_height;
    app.visible_height = inner_height;

    // Calculate scroll position (scroll_offset=0 means bottom)
    let max_scroll = total_height.saturating_sub(inner_height);
    let scroll_pos = max_scroll.saturating_sub(app.scroll_offset);

    let chat = chat.scroll((scroll_pos, 0));

    f.render_widget(chat, area);
}

fn draw_input(f: &mut Frame, app: &mut App, area: Rect) {
    // Store inner width for auto-wrap (subtract borders + 1 for cursor)
    app.input_width = area.width.saturating_sub(3);

    let (title, border_color, placeholder) = if app.pending_question.is_some() {
        (" reply > ", theme::WARNING, "Type your response... (Enter to send)")
    } else {
        (" > ", theme::ACCENT, "Type your message... (Enter to send, Esc to quit)")
    };

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ));

    app.input.set_block(input_block);
    app.input.set_placeholder_text(placeholder);
    app.input
        .set_style(Style::default().fg(theme::TEXT));
    app.input
        .set_cursor_style(Style::default().fg(theme::ACCENT).add_modifier(Modifier::REVERSED));

    f.render_widget(&app.input, area);
}

fn draw_status_line(f: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(theme::SUBTEXT);
    let accent = Style::default().fg(theme::ACCENT);

    let usage = &app.usage;
    let msg = usage.message_count;
    let threshold = usage.summary_threshold;

    // Progress bar
    let bar_width = 20usize;
    let filled = if threshold > 0 {
        ((msg as f64 / threshold as f64) * bar_width as f64).min(bar_width as f64) as usize
    } else {
        0
    };
    let empty = bar_width - filled;
    let bar = format!(
        "\u{2590}{}{}\u{258c}",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty)
    );

    // Tokens
    let total_tokens = usage.total_input_tokens + usage.total_output_tokens;
    let tokens_str = format_token_count(total_tokens);
    let input_str = format_token_count(usage.total_input_tokens);
    let output_str = format_token_count(usage.total_output_tokens);

    // Cost
    let cost_str = match (app.input_price_per_1m, app.output_price_per_1m) {
        (Some(ip), Some(op)) => {
            let cost = (usage.total_input_tokens as f64 / 1_000_000.0) * ip
                + (usage.total_output_tokens as f64 / 1_000_000.0) * op;
            format!("~${:.2}", cost)
        }
        _ => String::new(),
    };

    let mut spans = vec![
        Span::styled(" ", dim),
        Span::styled(format!("{}/{}", msg, threshold), accent),
        Span::styled(" ", dim),
        Span::styled(bar, dim),
        Span::styled(
            format!(" \u{2502} {} tok (\u{2191}{} \u{2193}{})", tokens_str, input_str, output_str),
            dim,
        ),
    ];

    if !cost_str.is_empty() {
        spans.push(Span::styled(format!(" \u{2502} {}", cost_str), dim));
    }

    let line = Line::from(spans);
    let status = Paragraph::new(line).style(Style::default().bg(theme::SURFACE));
    f.render_widget(status, area);
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{}", tokens)
    }
}

fn draw_model_selector(f: &mut Frame, app: &App) {
    let area = f.area();

    let add_model_label = "+ Add model";

    // Calculate popup size (+1 for the add model entry)
    let max_name_len = app
        .available_models
        .iter()
        .map(|m| m.name.len())
        .max()
        .unwrap_or(10)
        .max(add_model_label.len());
    let popup_width = (max_name_len as u16 + 8).min(area.width.saturating_sub(4)).max(20);
    let popup_height =
        (app.available_models.len() as u16 + 3).min(area.height.saturating_sub(4));

    // Center the popup
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    // Clear the area behind the popup
    f.render_widget(Clear, popup_area);

    let mut lines: Vec<Line<'static>> = Vec::new();

    for (i, model) in app.available_models.iter().enumerate() {
        let is_current = model.id == app.current_model_id;
        let is_selected = i == app.model_selector_index;

        let marker = if is_current { "* " } else { "  " };

        let style = if is_selected {
            Style::default()
                .fg(theme::SURFACE)
                .bg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else if is_current {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::TEXT)
        };

        lines.push(Line::from(Span::styled(
            format!("{}{}", marker, model.name),
            style,
        )));
    }

    // "+ Add model" entry
    let add_index = app.available_models.len();
    let is_add_selected = app.model_selector_index == add_index;
    let add_style = if is_add_selected {
        Style::default()
            .fg(theme::SURFACE)
            .bg(theme::SUCCESS)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::SUCCESS)
    };
    lines.push(Line::from(Span::styled(
        format!("  {}", add_model_label),
        add_style,
    )));

    let popup = Paragraph::new(lines).block(
        Block::default()
            .title(Span::styled(
                " Select Model ",
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::ACCENT)),
    );

    f.render_widget(popup, popup_area);
}

fn draw_add_model_form(f: &mut Frame, app: &App) {
    let form = match &app.add_model_form {
        Some(f) => f,
        None => return,
    };

    let area = f.area();
    let popup_width = 50u16.min(area.width.saturating_sub(4));
    let popup_height = 12u16.min(area.height.saturating_sub(4));

    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    let mut lines: Vec<Line<'static>> = Vec::new();

    let dimmed = Style::default().fg(theme::SUBTEXT);
    let active = Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(theme::ACCENT);

    // Step 1: Provider
    match form.step {
        AddModelStep::Provider => {
            lines.push(Line::from(Span::styled("Provider:", label_style)));
            for (i, option) in PROVIDER_OPTIONS.iter().enumerate() {
                let prefix = if i == form.provider_index { "> " } else { "  " };
                let style = if i == form.provider_index { active } else { dimmed };
                lines.push(Line::from(Span::styled(
                    format!("{}{}", prefix, option),
                    style,
                )));
            }
        }
        _ => {
            lines.push(Line::from(vec![
                Span::styled("Provider: ", dimmed),
                Span::styled(
                    PROVIDER_OPTIONS[form.provider_index].to_string(),
                    dimmed,
                ),
            ]));
        }
    }

    // Step 2: Model ID
    match form.step {
        AddModelStep::Provider => {}
        AddModelStep::ModelId => {
            lines.push(Line::from(Span::styled("Model ID:", label_style)));
            if form.provider_index == 2 {
                lines.push(Line::from(Span::styled(
                    "  (o4-mini, o3, gpt-4.1)",
                    dimmed,
                )));
            }
            lines.push(Line::from(Span::styled(
                format!("> {}_", form.input_buffer),
                active,
            )));
        }
        _ => {
            lines.push(Line::from(vec![
                Span::styled("Model ID: ", dimmed),
                Span::styled(form.model_id.clone(), dimmed),
            ]));
        }
    }

    // Step 3: Display Name
    match form.step {
        AddModelStep::Provider | AddModelStep::ModelId => {}
        AddModelStep::DisplayName => {
            lines.push(Line::from(Span::styled("Display Name:", label_style)));
            lines.push(Line::from(Span::styled(
                format!("> {}_", form.input_buffer),
                active,
            )));
        }
        _ => {
            lines.push(Line::from(vec![
                Span::styled("Display Name: ", dimmed),
                Span::styled(form.display_name.clone(), dimmed),
            ]));
        }
    }

    // Step 4: Base URL
    match form.step {
        AddModelStep::Provider | AddModelStep::ModelId | AddModelStep::DisplayName => {}
        AddModelStep::BaseUrl => {
            lines.push(Line::from(Span::styled(
                "Base URL (optional):",
                label_style,
            )));
            lines.push(Line::from(Span::styled(
                format!("> {}_", form.input_buffer),
                active,
            )));
        }
        _ => {
            if !form.base_url.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("Base URL: ", dimmed),
                    Span::styled(form.base_url.clone(), dimmed),
                ]));
            }
        }
    }

    // Step 5: API Key
    match form.step {
        AddModelStep::Provider
        | AddModelStep::ModelId
        | AddModelStep::DisplayName
        | AddModelStep::BaseUrl => {}
        AddModelStep::ApiKey => {
            lines.push(Line::from(Span::styled(
                "API Key (optional):",
                label_style,
            )));
            let masked: String = "*".repeat(form.input_buffer.len());
            lines.push(Line::from(Span::styled(
                format!("> {}_", masked),
                active,
            )));
        }
        AddModelStep::EnvVarName => {
            if !form.api_key.is_empty() {
                let masked: String = "*".repeat(form.api_key.len().min(8));
                lines.push(Line::from(vec![
                    Span::styled("API Key: ", dimmed),
                    Span::styled(format!("{}...", masked), dimmed),
                ]));
            }
        }
    }

    // Step 6: Env Var Name
    if let AddModelStep::EnvVarName = form.step {
        lines.push(Line::from(Span::styled(
            "Env variable name:",
            label_style,
        )));
        lines.push(Line::from(Span::styled(
            format!("> {}_", form.input_buffer),
            active,
        )));
    }

    // Footer
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Enter: next  Esc: cancel",
        dimmed,
    )));

    let popup = Paragraph::new(lines).block(
        Block::default()
            .title(Span::styled(
                " Add Model ",
                Style::default()
                    .fg(theme::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::SUCCESS)),
    );

    f.render_widget(popup, popup_area);
}

fn draw_left_panel(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.left_panel_focused;
    let border_color = if focused { theme::ACCENT } else { theme::BORDER };

    let block = Block::default()
        .title(Span::styled(
            " Sidebar ",
            Style::default().fg(border_color).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let dim = Style::default().fg(theme::SUBTEXT);
    let section_style = Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD);

    // Models section
    let models_active = app.left_panel_section == LeftPanelSection::Models;
    let models_header = if models_active { "▸ Models" } else { "▹ Models" };
    lines.push(Line::from(Span::styled(models_header, section_style)));

    if models_active {
        for (i, model) in app.available_models.iter().enumerate() {
            let is_current = model.id == app.current_model_id;
            let is_selected = focused && i == app.left_panel_index;
            let marker = if is_current { "* " } else { "  " };

            let style = if is_selected {
                Style::default().fg(theme::SURFACE).bg(theme::ACCENT).add_modifier(Modifier::BOLD)
            } else if is_current {
                Style::default().fg(theme::ACCENT)
            } else {
                Style::default().fg(theme::TEXT)
            };

            let name = truncate_str(&format!("{}{}", marker, model.name), inner.width as usize - 1);
            lines.push(Line::from(Span::styled(name, style)));
        }
    }

    lines.push(Line::default());

    // System Files section
    let sysfiles_active = app.left_panel_section == LeftPanelSection::SystemFiles;
    let sysfiles_header = if sysfiles_active { "▸ System Files" } else { "▹ System Files" };
    lines.push(Line::from(Span::styled(sysfiles_header, section_style)));

    if sysfiles_active {
        for (i, file) in app.system_files.iter().enumerate() {
            let is_selected = focused && i == app.left_panel_index;
            let style = if is_selected {
                Style::default().fg(theme::SURFACE).bg(theme::ACCENT).add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            lines.push(Line::from(Span::styled(format!("  {file}"), style)));
        }
    }

    lines.push(Line::default());

    // SubAgents section
    let subagents_active = app.left_panel_section == LeftPanelSection::SubAgents;
    let subagents_header = if subagents_active { "▸ SubAgents" } else { "▹ SubAgents" };
    lines.push(Line::from(Span::styled(subagents_header, section_style)));

    if subagents_active {
        if app.subagent_files.is_empty() {
            lines.push(Line::from(Span::styled("  (none)", dim)));
        } else {
            for (i, file) in app.subagent_files.iter().enumerate() {
                let is_selected = focused && i == app.left_panel_index;
                let name = file.trim_end_matches(".md");
                let style = if is_selected {
                    Style::default().fg(theme::SURFACE).bg(theme::ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    dim
                };
                lines.push(Line::from(Span::styled(format!("  {name}"), style)));
            }
        }
    }

    let panel = Paragraph::new(lines);
    f.render_widget(panel, inner);
}

fn draw_right_panel(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " Events ",
            Style::default().fg(theme::BORDER).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::BORDER));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.right_panel_events.is_empty() {
        let placeholder = Paragraph::new(Line::from(Span::styled(
            "No events yet",
            Style::default().fg(theme::SUBTEXT),
        )));
        f.render_widget(placeholder, inner);
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let dim = Style::default().fg(theme::SUBTEXT);

    for entry in &app.right_panel_events {
        // Timestamp + icon + kind
        let kind_style = if entry.kind.starts_with("Tool") || entry.kind.starts_with("WF:") {
            Style::default().fg(theme::TOOL)
        } else if entry.kind.contains("LLM") {
            Style::default().fg(theme::ACCENT)
        } else if entry.kind == "Intention" || entry.kind == "Contract" {
            Style::default().fg(theme::WARNING)
        } else if entry.kind == "Response" || entry.kind == "Completed" {
            Style::default().fg(theme::SUCCESS)
        } else {
            Style::default().fg(theme::TEXT)
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{} ", entry.timestamp), dim),
            Span::styled(format!("{} ", entry.icon), kind_style),
            Span::styled(entry.kind.clone(), kind_style),
        ]));

        if !entry.detail.is_empty() {
            let detail = truncate_str(&entry.detail, inner.width.saturating_sub(4) as usize);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(detail, dim),
            ]));
        }
    }

    // Auto-scroll to bottom
    let total_height = lines.len() as u16;
    let visible = inner.height;
    let scroll = total_height.saturating_sub(visible);

    let panel = Paragraph::new(lines).scroll((scroll, 0));
    f.render_widget(panel, inner);
}

fn draw_hint_bar(f: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(theme::SUBTEXT);
    let key_style = Style::default().fg(theme::ACCENT);

    let hints = if app.left_panel_focused {
        vec![
            Span::styled(" F1", key_style), Span::styled(":Hide ", dim),
            Span::styled("Tab", key_style), Span::styled(":Section ", dim),
            Span::styled("Up/Dn", key_style), Span::styled(":Nav ", dim),
            Span::styled("Enter", key_style), Span::styled(":Select ", dim),
            Span::styled("Esc", key_style), Span::styled(":Back", dim),
        ]
    } else {
        let mut h = vec![
            Span::styled(" F1", key_style), Span::styled(":Sidebar", dim),
        ];
        if app.left_panel_visible {
            h.push(Span::styled("*", Style::default().fg(theme::SUCCESS)));
        }
        h.extend([
            Span::styled(" F2", key_style), Span::styled(":Events", dim),
        ]);
        if app.right_panel_visible {
            h.push(Span::styled("*", Style::default().fg(theme::SUCCESS)));
        }
        h.extend([
            Span::styled(" ^M", key_style), Span::styled(":Model ", dim),
            Span::styled("^Y", key_style), Span::styled(":Copy ", dim),
            Span::styled("Esc", key_style), Span::styled(":Quit", dim),
        ]);
        h
    };

    let line = Line::from(hints);
    let bar = Paragraph::new(line).style(Style::default().bg(theme::SURFACE));
    f.render_widget(bar, area);
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max_len);
        format!("{}...", &s[..end])
    }
}
