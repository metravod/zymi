use std::sync::LazyLock;

use pulldown_cmark::{Event, Parser, Tag, TagEnd, CodeBlockKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::easy::HighlightLines;

use super::theme;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

pub fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let parser = Parser::new(text);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();

    let mut in_code_block = false;
    let mut code_block_lang = String::new();
    let mut code_block_content = String::new();

    let mut bold = false;
    let mut italic = false;
    let mut heading_level: Option<u8> = None;
    let mut list_depth: usize = 0;

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                flush_line(&mut current_spans, &mut lines);
                heading_level = Some(level as u8);
            }
            Event::End(TagEnd::Heading(_)) => {
                flush_line(&mut current_spans, &mut lines);
                heading_level = None;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush_line(&mut current_spans, &mut lines);
                in_code_block = true;
                code_block_content.clear();
                code_block_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    _ => String::new(),
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                let highlighted = highlight_code(&code_block_content, &code_block_lang);
                lines.extend(highlighted);
                code_block_content.clear();
                code_block_lang.clear();
            }
            Event::Start(Tag::Strong) => bold = true,
            Event::End(TagEnd::Strong) => bold = false,
            Event::Start(Tag::Emphasis) => italic = true,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::Code(code) => {
                current_spans.push(Span::styled(
                    format!(" {} ", code),
                    Style::default().fg(theme::ACCENT).bg(theme::SURFACE),
                ));
            }
            Event::Start(Tag::List(_)) => {
                list_depth += 1;
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
            }
            Event::Start(Tag::Item) => {
                flush_line(&mut current_spans, &mut lines);
                let indent = "  ".repeat(list_depth.saturating_sub(1));
                current_spans.push(Span::styled(
                    format!("{}• ", indent),
                    Style::default().fg(theme::SUBTEXT),
                ));
            }
            Event::End(TagEnd::Item) => {
                flush_line(&mut current_spans, &mut lines);
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush_line(&mut current_spans, &mut lines);
                lines.push(Line::default());
            }
            Event::Text(text) => {
                if in_code_block {
                    code_block_content.push_str(&text);
                } else {
                    let style = if let Some(level) = heading_level {
                        match level {
                            1 => Style::default()
                                .fg(theme::ACCENT)
                                .add_modifier(Modifier::BOLD),
                            2 => Style::default()
                                .fg(theme::ACCENT)
                                .add_modifier(Modifier::BOLD),
                            _ => Style::default()
                                .fg(theme::TEXT)
                                .add_modifier(Modifier::BOLD),
                        }
                    } else {
                        let mut s = Style::default().fg(theme::TEXT);
                        if bold {
                            s = s.add_modifier(Modifier::BOLD);
                        }
                        if italic {
                            s = s.add_modifier(Modifier::ITALIC);
                        }
                        s
                    };

                    let prefix = if let Some(level) = heading_level {
                        let hashes = "#".repeat(level as usize);
                        format!("{} ", hashes)
                    } else {
                        String::new()
                    };

                    if !prefix.is_empty() {
                        current_spans.push(Span::styled(
                            prefix,
                            Style::default().fg(theme::SUBTEXT),
                        ));
                    }
                    current_spans.push(Span::styled(text.to_string(), style));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut current_spans, &mut lines);
            }
            _ => {}
        }
    }

    flush_line(&mut current_spans, &mut lines);

    // Trim trailing empty lines
    while lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }

    lines
}

fn flush_line(spans: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

fn highlight_code(code: &str, lang: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let border_style = Style::default().fg(theme::BORDER);
    let code_trimmed = code.trim_end_matches('\n');

    // Find the longest line for border width
    let max_width = code_trimmed.lines().map(|l| l.len()).max().unwrap_or(0);
    let border_width = max_width.max(20);

    // Top border with language
    let lang_display = if lang.is_empty() { "" } else { lang };
    let top_border = if lang_display.is_empty() {
        format!("  ┌{}┐", "─".repeat(border_width + 2))
    } else {
        format!(
            "  ┌─ {} {}┐",
            lang_display,
            "─".repeat(border_width.saturating_sub(lang_display.len() + 1))
        )
    };
    lines.push(Line::from(Span::styled(top_border, border_style)));

    let ss = &*SYNTAX_SET;
    let theme = &THEME_SET.themes["base16-ocean.dark"];

    let syntax = ss
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let mut h = HighlightLines::new(syntax, theme);

    for line_text in code_trimmed.lines() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled("  │ ", border_style));

        match h.highlight_line(line_text, ss) {
            Ok(ranges) => {
                let mut line_len = 0;
                for (style, text) in ranges {
                    let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                    spans.push(Span::styled(text.to_string(), Style::default().fg(fg)));
                    line_len += text.len();
                }
                // Pad to border width
                let pad = border_width.saturating_sub(line_len);
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
            }
            Err(_) => {
                let pad = border_width.saturating_sub(line_text.len());
                spans.push(Span::styled(
                    line_text.to_string(),
                    Style::default().fg(theme::TEXT),
                ));
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
            }
        }

        spans.push(Span::styled(" │", border_style));
        lines.push(Line::from(spans));
    }

    // Bottom border
    let bottom_border = format!("  └{}┘", "─".repeat(border_width + 2));
    lines.push(Line::from(Span::styled(bottom_border, border_style)));

    lines
}
