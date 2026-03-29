use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};
use tui_textarea::{CursorMove, TextArea};

use super::app::{AddModelForm, AddModelStep, App, LeftPanelSection, PROVIDER_OPTIONS};

pub struct NewModelInfo {
    pub provider_index: usize,
    pub model_id: String,
    pub display_name: String,
    pub base_url: String,
    pub api_key: String,
    pub env_var_name: String,
}

pub enum InputAction {
    SendMessage(String),
    SwitchModel(String),
    AddModel(NewModelInfo),
    OpenEditor(PathBuf),
    ToggleCopyMode,
    Interrupt,
    Quit,
    None,
}

pub fn handle_event(app: &mut App, event: Event) -> InputAction {
    // Handle mouse events
    if let Event::Mouse(mouse) = &event {
        let in_right_panel = app.right_panel_visible
            && mouse.column >= app.right_panel_x_range.0
            && mouse.column < app.right_panel_x_range.1;

        return match mouse.kind {
            MouseEventKind::ScrollUp => {
                if in_right_panel {
                    app.right_panel_scroll = app.right_panel_scroll.saturating_add(3);
                    let max = app.right_panel_total_lines.saturating_sub(app.right_panel_visible_height);
                    if app.right_panel_scroll > max {
                        app.right_panel_scroll = max;
                    }
                    app.right_panel_auto_scroll = false;
                } else {
                    app.scroll_up(3);
                }
                InputAction::None
            }
            MouseEventKind::ScrollDown => {
                if in_right_panel {
                    app.right_panel_scroll = app.right_panel_scroll.saturating_sub(3);
                    if app.right_panel_scroll == 0 {
                        app.right_panel_auto_scroll = true;
                    }
                } else {
                    app.scroll_down(3);
                }
                InputAction::None
            }
            _ => InputAction::None,
        };
    }

    let key = match &event {
        Event::Key(k) => *k,
        _ => return InputAction::None,
    };

    // Handle approval responses first
    if app.pending_approval.is_some() {
        match key.code {
            KeyCode::Left | KeyCode::Right => {
                app.approval_selected = !app.approval_selected;
                return InputAction::None;
            }
            KeyCode::Enter => {
                app.handle_approval(app.approval_selected);
                return InputAction::None;
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.handle_approval(true);
                return InputAction::None;
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                app.handle_approval(false);
                return InputAction::None;
            }
            // Esc rejects (same as No)
            KeyCode::Esc => {
                app.handle_approval(false);
                return InputAction::None;
            }
            // Allow panel toggles during approval
            KeyCode::F(1) | KeyCode::F(2) => { /* fall through to global handlers */ }
            _ => return InputAction::None,
        }
    }

    // Handle pending question from agent (ask_user tool)
    if app.pending_question.is_some() {
        match key.code {
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                let text = app.get_input_and_clear();
                if !text.trim().is_empty() {
                    app.handle_question_response(text);
                }
                return InputAction::None;
            }
            // Esc cancels the question
            KeyCode::Esc => {
                app.handle_question_response("[cancelled by user]".to_string());
                return InputAction::None;
            }
            // Allow panel toggles during question input
            KeyCode::F(1) | KeyCode::F(2) => { /* fall through to global handlers */ }
            _ => {
                app.input.input(event);
                auto_wrap_input(&mut app.input, app.input_width as usize);
                return InputAction::None;
            }
        }
    }

    // Handle add model form when open
    if app.add_model_form.is_some() {
        return handle_add_model_form(app, key.code);
    }

    // Handle model selector when open
    if app.model_selector_open {
        return match key.code {
            KeyCode::Esc => {
                app.model_selector_open = false;
                InputAction::None
            }
            KeyCode::Up => {
                if app.model_selector_index > 0 {
                    app.model_selector_index -= 1;
                }
                InputAction::None
            }
            KeyCode::Down => {
                // +1 for the "Add model" entry
                if app.model_selector_index + 1 < app.available_models.len() + 1 {
                    app.model_selector_index += 1;
                }
                InputAction::None
            }
            KeyCode::Enter => {
                if app.model_selector_index == app.available_models.len() {
                    // "Add model" entry selected
                    app.model_selector_open = false;
                    app.add_model_form = Some(AddModelForm::new());
                    return InputAction::None;
                }
                app.model_selector_open = false;
                let model_id = app.available_models[app.model_selector_index].id.clone();
                if model_id != app.current_model_id {
                    return InputAction::SwitchModel(model_id);
                }
                InputAction::None
            }
            _ => InputAction::None,
        };
    }

    // Left panel navigation when focused
    if app.left_panel_focused && app.left_panel_visible {
        return match key.code {
            // Up/Down navigates items within the current section
            KeyCode::Up => {
                if app.left_panel_index > 0 {
                    app.left_panel_index -= 1;
                }
                InputAction::None
            }
            KeyCode::Down => {
                let max = app.left_panel_section_len().saturating_sub(1);
                if app.left_panel_index < max {
                    app.left_panel_index += 1;
                }
                InputAction::None
            }
            // Tab/BackTab switches between sections (Models / Files / SubAgents)
            KeyCode::Tab => {
                app.left_panel_section = app.left_panel_section.next();
                app.left_panel_index = 0;
                InputAction::None
            }
            KeyCode::BackTab => {
                app.left_panel_section = app.left_panel_section.prev();
                app.left_panel_index = 0;
                InputAction::None
            }
            // Right arrow moves focus to chat
            KeyCode::Right => {
                app.left_panel_focused = false;
                InputAction::None
            }
            KeyCode::Enter => {
                match app.left_panel_section {
                    LeftPanelSection::Models => {
                        // Last entry is "+ Add model"
                        if app.left_panel_index == app.available_models.len() {
                            app.left_panel_focused = false;
                            app.add_model_form = Some(AddModelForm::new());
                            return InputAction::None;
                        }
                        if let Some(model) = app.available_models.get(app.left_panel_index) {
                            let mid = model.id.clone();
                            if mid != app.current_model_id {
                                return InputAction::SwitchModel(mid);
                            }
                        }
                        InputAction::None
                    }
                    LeftPanelSection::SystemFiles | LeftPanelSection::SubAgents => {
                        if let Some(path) = app.left_panel_selected_path() {
                            InputAction::OpenEditor(path)
                        } else {
                            InputAction::None
                        }
                    }
                }
            }
            // Q quits from sidebar
            KeyCode::Char('q') if !app.is_processing => {
                InputAction::Quit
            }
            KeyCode::Esc | KeyCode::F(1) => {
                app.left_panel_focused = false;
                InputAction::None
            }
            _ => InputAction::None,
        };
    }

    // Right panel navigation when focused
    if app.right_panel_focused && app.right_panel_visible {
        return match key.code {
            KeyCode::Up => {
                if app.right_panel_selected > 0 {
                    app.right_panel_selected -= 1;
                    app.right_panel_auto_scroll = false;
                }
                InputAction::None
            }
            KeyCode::Down => {
                let max = app.right_panel_events.len().saturating_sub(1);
                if app.right_panel_selected < max {
                    app.right_panel_selected += 1;
                }
                InputAction::None
            }
            KeyCode::Enter => {
                let idx = app.right_panel_selected;
                if app.right_panel_expanded.contains(&idx) {
                    app.right_panel_expanded.remove(&idx);
                } else {
                    app.right_panel_expanded.insert(idx);
                }
                InputAction::None
            }
            KeyCode::Left => {
                app.right_panel_focused = false;
                InputAction::None
            }
            KeyCode::Esc | KeyCode::F(2) => {
                app.right_panel_focused = false;
                InputAction::None
            }
            _ => InputAction::None,
        };
    }

    match key.code {
        // Left arrow: focus left panel (if visible)
        KeyCode::Left if app.left_panel_visible && !app.left_panel_focused && key.modifiers.is_empty() => {
            app.left_panel_focused = true;
            app.right_panel_focused = false;
            InputAction::None
        }
        // F1 toggles left panel
        KeyCode::F(1) => {
            app.left_panel_visible = !app.left_panel_visible;
            app.left_panel_focused = app.left_panel_visible;
            InputAction::None
        }
        // F2 toggles right panel + focus
        KeyCode::F(2) => {
            if app.right_panel_focused {
                app.right_panel_focused = false;
                app.right_panel_visible = false;
            } else if app.right_panel_visible {
                app.right_panel_focused = true;
                // Jump selection to last event
                app.right_panel_selected = app.right_panel_events.len().saturating_sub(1);
            } else {
                app.right_panel_visible = true;
                app.right_panel_focused = true;
                app.right_panel_selected = app.right_panel_events.len().saturating_sub(1);
            }
            InputAction::None
        }
        // Right arrow: focus right panel (if visible and not in left panel)
        KeyCode::Right if app.right_panel_visible && !app.right_panel_focused && !app.left_panel_focused && key.modifiers.is_empty() => {
            app.right_panel_focused = true;
            app.right_panel_selected = app.right_panel_events.len().saturating_sub(1);
            InputAction::None
        }
        // Ctrl+Y toggles copy mode (disables mouse capture for text selection)
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            InputAction::ToggleCopyMode
        }
        // Ctrl+M toggles model selector (blocked while processing)
        KeyCode::Char('m') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !app.is_processing {
                app.model_selector_open = true;
                // Set index to current model
                app.model_selector_index = app
                    .available_models
                    .iter()
                    .position(|m| m.id == app.current_model_id)
                    .unwrap_or(0);
            }
            InputAction::None
        }
        // Esc interrupts agent processing
        KeyCode::Esc => {
            if app.is_processing {
                InputAction::Interrupt
            } else {
                InputAction::None
            }
        }
        KeyCode::Enter => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.input.insert_newline();
                InputAction::None
            } else {
                let text = app.get_input_and_clear();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return InputAction::None;
                }
                if app.handle_command(trimmed) {
                    if app.should_quit {
                        return InputAction::Quit;
                    }
                    return InputAction::None;
                }
                if app.is_processing {
                    return InputAction::None;
                }
                InputAction::SendMessage(text)
            }
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.scroll_up(1);
            InputAction::None
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.scroll_down(1);
            InputAction::None
        }
        KeyCode::PageUp => {
            app.scroll_up(10);
            InputAction::None
        }
        KeyCode::PageDown => {
            app.scroll_down(10);
            InputAction::None
        }
        // Filter out mouse escape sequence fragments that can leak over SSH.
        // SGR mouse sequences contain [, <, ;, M, m as part of \x1b[<Cb;Cx;CyM.
        // If crossterm can't parse them, they arrive as individual Char events.
        KeyCode::Char(c)
            if key.modifiers.is_empty()
                && matches!(c, '[' | '<' | 'M' | 'm')
                && app.is_processing =>
        {
            InputAction::None
        }
        _ => {
            app.input.input(event);
            auto_wrap_input(&mut app.input, app.input_width as usize);
            InputAction::None
        }
    }
}

fn auto_wrap_input(input: &mut TextArea<'static>, max_width: usize) {
    let wrap_at = max_width.saturating_sub(2);
    if wrap_at < 4 {
        return;
    }

    let (cur_row, cur_col) = input.cursor();
    let lines = input.lines();
    if cur_row >= lines.len() {
        return;
    }

    let char_count = lines[cur_row].chars().count();
    if char_count <= wrap_at {
        return;
    }

    // Find last space within wrap_at range to break at word boundary
    let chars: Vec<char> = lines[cur_row].chars().collect();
    let mut break_at = wrap_at;
    for i in (1..wrap_at).rev() {
        if chars[i] == ' ' {
            break_at = i + 1;
            break;
        }
    }

    // Insert newline at break point
    input.move_cursor(CursorMove::Jump(cur_row as u16, break_at as u16));
    input.insert_newline();

    // Restore cursor to correct logical position
    if cur_col >= break_at {
        input.move_cursor(CursorMove::Jump(
            (cur_row + 1) as u16,
            (cur_col - break_at) as u16,
        ));
    } else {
        input.move_cursor(CursorMove::Jump(cur_row as u16, cur_col as u16));
    }
}

fn handle_add_model_form(app: &mut App, key: KeyCode) -> InputAction {
    let form = app.add_model_form.as_mut().unwrap();

    if key == KeyCode::Esc {
        app.add_model_form = None;
        return InputAction::None;
    }

    match form.step {
        AddModelStep::Provider => match key {
            KeyCode::Up => {
                if form.provider_index > 0 {
                    form.provider_index -= 1;
                }
            }
            KeyCode::Down => {
                if form.provider_index + 1 < PROVIDER_OPTIONS.len() {
                    form.provider_index += 1;
                }
            }
            KeyCode::Enter => {
                // ChatGPT OAuth: skip all steps — models are fetched automatically
                if form.provider_index == 2 {
                    let info = NewModelInfo {
                        provider_index: 2,
                        model_id: String::new(),
                        display_name: String::new(),
                        base_url: String::new(),
                        api_key: String::new(),
                        env_var_name: String::new(),
                    };
                    app.add_model_form = None;
                    return InputAction::AddModel(info);
                }

                form.step = AddModelStep::ModelId;
                // Pre-fill sensible defaults per provider
                form.input_buffer = match form.provider_index {
                    1 => "claude-sonnet-4-20250514".to_string(),
                    _ => String::new(),
                };
            }
            _ => {}
        },
        AddModelStep::ModelId => match key {
            KeyCode::Char(c) => {
                form.input_buffer.push(c);
            }
            KeyCode::Backspace => {
                form.input_buffer.pop();
            }
            KeyCode::Enter => {
                if !form.input_buffer.trim().is_empty() {
                    form.model_id = form.input_buffer.trim().to_string();
                    form.input_buffer = form.model_id.clone();
                    form.step = AddModelStep::DisplayName;
                }
            }
            _ => {}
        },
        AddModelStep::DisplayName => match key {
            KeyCode::Char(c) => {
                form.input_buffer.push(c);
            }
            KeyCode::Backspace => {
                form.input_buffer.pop();
            }
            KeyCode::Enter => {
                let name = form.input_buffer.trim().to_string();
                form.display_name = if name.is_empty() {
                    form.model_id.clone()
                } else {
                    name
                };
                form.input_buffer.clear();
                form.step = AddModelStep::BaseUrl;
            }
            _ => {}
        },
        AddModelStep::BaseUrl => match key {
            KeyCode::Char(c) => {
                form.input_buffer.push(c);
            }
            KeyCode::Backspace => {
                form.input_buffer.pop();
            }
            KeyCode::Enter => {
                form.base_url = form.input_buffer.trim().to_string();
                form.input_buffer.clear();
                form.step = AddModelStep::ApiKey;
            }
            _ => {}
        },
        AddModelStep::ApiKey => match key {
            KeyCode::Char(c) => {
                form.input_buffer.push(c);
            }
            KeyCode::Backspace => {
                form.input_buffer.pop();
            }
            KeyCode::Enter => {
                form.api_key = form.input_buffer.trim().to_string();
                if form.api_key.is_empty() {
                    // No key needed (e.g. local Ollama), submit directly
                    let info = NewModelInfo {
                        provider_index: form.provider_index,
                        model_id: form.model_id.clone(),
                        display_name: form.display_name.clone(),
                        base_url: form.base_url.clone(),
                        api_key: String::new(),
                        env_var_name: String::new(),
                    };
                    app.add_model_form = None;
                    return InputAction::AddModel(info);
                }
                // Suggest default env var name based on provider
                let default = if form.provider_index == 1 {
                    "ANTHROPIC_API_KEY"
                } else {
                    "OPENAI_API_KEY"
                };
                form.input_buffer = default.to_string();
                form.step = AddModelStep::EnvVarName;
            }
            _ => {}
        },
        AddModelStep::EnvVarName => match key {
            KeyCode::Char(c) => {
                form.input_buffer.push(c);
            }
            KeyCode::Backspace => {
                form.input_buffer.pop();
            }
            KeyCode::Enter => {
                form.env_var_name = form.input_buffer.trim().to_string();
                if form.env_var_name.is_empty() {
                    // Require a name
                    return InputAction::None;
                }
                let info = NewModelInfo {
                    provider_index: form.provider_index,
                    model_id: form.model_id.clone(),
                    display_name: form.display_name.clone(),
                    base_url: form.base_url.clone(),
                    api_key: form.api_key.clone(),
                    env_var_name: form.env_var_name.clone(),
                };
                app.add_model_form = None;
                return InputAction::AddModel(info);
            }
            _ => {}
        },
    }

    InputAction::None
}
