use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Tabs,
};

use mcpr_session::MemorySessionStore;

use super::state::{ConnectionStatus, SharedTuiState, Tab};

pub fn render(frame: &mut Frame, state: &SharedTuiState, sessions: &MemorySessionStore) {
    let s = state.lock().unwrap();

    // Size left panel to fit the longest URL + label padding (10) + border (2) + margin (2)
    // Studio URLs append "/studio" (+7) to proxy and tunnel URLs
    let studio_extra = if s.tunnel_url.is_empty() { 0 } else { 7 };
    let longest_url = [&s.proxy_url, &s.tunnel_url, &s.mcp_upstream, &s.widgets]
        .iter()
        .map(|u| u.len())
        .max()
        .unwrap_or(20)
        + studio_extra;
    let ideal_width = (longest_url + 14) as u16;
    // Clamp: at least 36, at most 50% of terminal width
    let max_left = frame.area().width / 2;
    let left_width = ideal_width.clamp(36, max_left);

    let chunks = Layout::horizontal([Constraint::Length(left_width), Constraint::Min(40)])
        .split(frame.area());

    render_info_panel(frame, chunks[0], &s);

    // Right panel: tab bar + content
    let right_chunks =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(chunks[1]);

    let tab_titles = vec!["Requests", "Sessions"];
    let selected = match s.active_tab {
        Tab::Requests => 0,
        Tab::Sessions => 1,
    };
    let tabs = Tabs::new(tab_titles)
        .select(selected)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .divider("│");
    frame.render_widget(tabs, right_chunks[0]);

    match s.active_tab {
        Tab::Requests => render_log_panel(frame, right_chunks[1], &s),
        Tab::Sessions => render_sessions_panel(frame, right_chunks[1], sessions, &s),
    }
}

fn status_style(status: ConnectionStatus) -> (Color, &'static str) {
    match status {
        ConnectionStatus::Connected => (Color::Green, "●"),
        ConnectionStatus::Connecting => (Color::Yellow, "◐"),
        ConnectionStatus::Disconnected => (Color::Red, "○"),
        ConnectionStatus::Evicted => (Color::Magenta, "⇄"),
        ConnectionStatus::NotMcp => (Color::Yellow, "⚠"),
        ConnectionStatus::Unknown => (Color::DarkGray, "?"),
    }
}

fn status_line(label: &str, status: ConnectionStatus) -> Line<'static> {
    let (color, symbol) = status_style(status);
    Line::from(vec![
        Span::styled(
            format!("  {label:<10}"),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(symbol.to_string(), Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(status.label().to_string(), Style::default().fg(color)),
    ])
}

fn render_info_panel(frame: &mut Frame, area: Rect, s: &super::state::TuiState) {
    let block = Block::default()
        .title(" mcpr proxy ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    // Truncate long URLs to fit panel
    let max_url_len = area.width.saturating_sub(12) as usize;
    let truncate = |url: &str| -> String {
        if url.len() > max_url_len {
            format!("{}…", &url[..max_url_len.saturating_sub(1)])
        } else {
            url.to_string()
        }
    };

    let widgets_display = if let Some(count) = s.widget_count {
        format!("{} ({count} widgets)", truncate(&s.widgets))
    } else {
        truncate(&s.widgets)
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    let mut tunnel_spans = vec![
        Span::styled("  Tunnel  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            truncate(&s.tunnel_url),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if s.tunnel_anonymous {
        tunnel_spans.push(Span::styled(" (temp)", Style::default().fg(Color::Yellow)));
    }
    lines.push(Line::from(tunnel_spans));
    lines.push(Line::from(vec![
        Span::styled("  Proxy   ", Style::default().fg(Color::DarkGray)),
        Span::raw(truncate(&s.proxy_url)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  MCP     ", Style::default().fg(Color::DarkGray)),
        Span::raw(truncate(&s.mcp_upstream)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Widgets ", Style::default().fg(Color::DarkGray)),
        Span::raw(widgets_display),
    ]));
    lines.push(Line::from(""));

    lines.push(status_line("Tunnel", s.tunnel_status));
    lines.push(status_line("MCP", s.mcp_status));
    if let Some(ref warning) = s.mcp_warning {
        let indent = "            ";
        let max_warn_width = area.width.saturating_sub(2 + indent.len() as u16) as usize;
        if max_warn_width > 0 {
            for chunk in wrap_text(warning, max_warn_width) {
                lines.push(Line::from(vec![
                    Span::styled(indent, Style::default()),
                    Span::styled(chunk, Style::default().fg(Color::Yellow)),
                ]));
            }
        }
    }
    lines.push(status_line("Widgets", s.widgets_status));

    if s.tunnel_anonymous {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ⚠ Temporary tunnel — expires in 1 week",
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(Span::styled(
            "    Provide email to keep your subdomain",
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines.extend([
        Line::from(""),
        Line::from(vec![
            Span::styled("  Uptime  ", Style::default().fg(Color::DarkGray)),
            Span::raw(s.uptime()),
        ]),
        Line::from(vec![
            Span::styled("  Reqs    ", Style::default().fg(Color::DarkGray)),
            Span::raw(s.request_count.to_string()),
        ]),
    ]);

    if !s.widget_names.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Widgets found:",
            Style::default().fg(Color::DarkGray),
        )));
        for name in &s.widget_names {
            lines.push(Line::from(vec![
                Span::styled("    • ", Style::default().fg(Color::DarkGray)),
                Span::raw(name.clone()),
            ]));
        }
    }

    lines.push(Line::from(""));
    let studio_url = format!("{}/studio", s.proxy_url);
    lines.push(Line::from(vec![
        Span::styled("  Studio  ", Style::default().fg(Color::DarkGray)),
        Span::styled(studio_url, Style::default().fg(Color::Cyan)),
    ]));
    if !s.tunnel_url.is_empty() {
        let tunnel_studio_url = format!("{}/studio", s.tunnel_url);
        lines.push(Line::from(vec![
            Span::styled("          ", Style::default().fg(Color::DarkGray)),
            Span::styled(tunnel_studio_url, Style::default().fg(Color::Cyan)),
        ]));
    }

    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "  ctrl+c quit  ↑↓ scroll  tab switch",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Star us ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "github.com/cptrodgers/mcpr",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ]);

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn render_log_panel(frame: &mut Frame, area: Rect, s: &super::state::TuiState) {
    let block = Block::default()
        .title(" Requests ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    let visible_height = inner.height as usize;

    // Build log lines
    //
    // MCP requests:    HH:MM:SS POST  tools/call      200  1.2KB  45ms  rewritten
    // Other requests:  HH:MM:SS GET   /oauth/register  201  232B   8ms  rewritten
    let log_lines: Vec<Line> = s
        .log_entries
        .iter()
        .map(|entry| {
            let status_color = if entry.status < 300 {
                Color::Green
            } else if entry.status < 400 {
                Color::Yellow
            } else {
                Color::Red
            };

            let method_color = match entry.method.as_str() {
                "POST" => Color::Cyan,
                "GET" => Color::Green,
                "DELETE" => Color::Red,
                _ => Color::White,
            };

            // Layout: time  METHOD  status  size  upstream↑  proxy↓  label
            // Fixed-width columns first, variable-length label last (never truncated)

            let mut spans = vec![
                Span::styled(
                    format!(" {} ", entry.timestamp),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<5}", entry.method),
                    Style::default()
                        .fg(method_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<4}", entry.status),
                    Style::default().fg(status_color),
                ),
            ];

            // Size
            let size_str = match entry.resp_size {
                Some(size) => format!("{:>7}", format_bytes(size)),
                None => "      -".to_string(),
            };
            spans.push(Span::styled(
                format!("{size_str} "),
                Style::default().fg(Color::DarkGray),
            ));

            // Duration: total | upstream↑ proxy↓
            match (entry.duration_ms, entry.upstream_ms) {
                (Some(total), Some(upstream)) => {
                    let proxy = total.saturating_sub(upstream);
                    spans.push(Span::styled(
                        format!("{:>5} ", format_duration(total)),
                        Style::default()
                            .fg(duration_color(total))
                            .add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled(
                        format!("{:>5}↑", format_duration(upstream)),
                        Style::default().fg(duration_color(upstream)),
                    ));
                    spans.push(Span::styled(
                        format!("{:>5}↓ ", format_duration(proxy)),
                        Style::default().fg(Color::Cyan),
                    ));
                }
                (Some(total), None) => {
                    spans.push(Span::styled(
                        format!("{:>5} ", format_duration(total)),
                        Style::default()
                            .fg(duration_color(total))
                            .add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::raw("            "));
                }
                _ => {
                    spans.push(Span::styled(
                        "    -              ",
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }

            // Label — MCP method (yellow) or path, full length at the end
            if let Some(ref mcp) = entry.mcp_method {
                spans.push(Span::styled(
                    mcp.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
                // Detail: tool name, resource URI, etc.
                if let Some(ref detail) = entry.detail {
                    spans.push(Span::styled(
                        format!(" {detail}"),
                        Style::default().fg(Color::White),
                    ));
                }
            } else {
                spans.push(Span::raw(entry.path.clone()));
            }

            // JSON-RPC error (if response contains one)
            if let Some((code, ref msg)) = entry.jsonrpc_error {
                spans.push(Span::styled(
                    format!(" [{code} {msg}]"),
                    Style::default().fg(Color::Red),
                ));
            }

            // Upstream URL
            if let Some(ref url) = entry.upstream_url {
                spans.push(Span::styled(
                    format!(" → {url}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            // Note (rewritten, passthrough, sse, etc.)
            if !entry.note.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", entry.note),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            Line::from(spans)
        })
        .collect();

    let total = log_lines.len();

    // Calculate scroll position
    let scroll = if s.auto_scroll {
        total.saturating_sub(visible_height) as u16
    } else {
        s.scroll_offset
    };

    let paragraph = Paragraph::new(log_lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);

    // Scrollbar
    if total > visible_height {
        let mut scrollbar_state =
            ScrollbarState::new(total.saturating_sub(visible_height)).position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

fn render_sessions_panel(
    frame: &mut Frame,
    area: Rect,
    sessions: &MemorySessionStore,
    s: &super::state::TuiState,
) {
    let session_list = sessions.list_sync();

    if session_list.is_empty() {
        let block = Block::default()
            .title(" Sessions ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No active sessions",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Sessions appear when an MCP client sends an initialize request.",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
        return;
    }

    // Split: session list (top) + detail (bottom)
    let chunks = Layout::vertical([
        Constraint::Length(session_list.len() as u16 + 2),
        Constraint::Min(6),
    ])
    .split(area);

    // --- Top: session list with cursor ---
    let list_block = Block::default()
        .title(" Sessions ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let selected = s.selected_session.min(session_list.len().saturating_sub(1));

    let list_lines: Vec<Line> = session_list
        .iter()
        .enumerate()
        .map(|(i, session)| {
            let is_selected = i == selected;
            let state_color = session_state_color(&session.state);
            let state_label = session_state_label(&session.state);

            let marker = if is_selected { "▸ " } else { "  " };
            let id_style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(Color::White)
            };

            let client_str = session
                .client_info
                .as_ref()
                .map(|c| {
                    let v = c
                        .version
                        .as_deref()
                        .map(|v| format!(" v{v}"))
                        .unwrap_or_default();
                    format!("  {}{v}", c.name)
                })
                .unwrap_or_default();

            Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Cyan)),
                Span::styled(session.id.clone(), id_style),
                Span::raw("  "),
                Span::styled(
                    state_label,
                    Style::default()
                        .fg(state_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(client_str, Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(list_lines).block(list_block);
    frame.render_widget(paragraph, chunks[0]);

    // --- Bottom: selected session detail + its requests ---
    let session = &session_list[selected];
    render_session_detail(frame, chunks[1], session, s);
}

fn session_state_color(state: &mcpr_session::SessionState) -> Color {
    match state {
        mcpr_session::SessionState::Created => Color::Yellow,
        mcpr_session::SessionState::Initialized => Color::Cyan,
        mcpr_session::SessionState::Active => Color::Green,
        mcpr_session::SessionState::Closed => Color::Red,
    }
}

fn session_state_label(state: &mcpr_session::SessionState) -> &'static str {
    match state {
        mcpr_session::SessionState::Created => "CREATED",
        mcpr_session::SessionState::Initialized => "INITIALIZED",
        mcpr_session::SessionState::Active => "ACTIVE",
        mcpr_session::SessionState::Closed => "CLOSED",
    }
}

fn render_session_detail(
    frame: &mut Frame,
    area: Rect,
    session: &mcpr_session::SessionInfo,
    s: &super::state::TuiState,
) {
    let block = Block::default()
        .title(format!(" {} ", session.id))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    let visible_height = inner.height as usize;

    let mut lines: Vec<Line> = Vec::new();

    // Session info header
    let state_color = session_state_color(&session.state);
    let state_label = session_state_label(&session.state);

    lines.push(Line::from(vec![
        Span::styled("  State     ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            state_label,
            Style::default()
                .fg(state_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    if let Some(ref client) = session.client_info {
        let version = client
            .version
            .as_deref()
            .map(|v| format!(" v{v}"))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled("  Client    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}{}", client.name, version)),
        ]));
    }

    lines.push(Line::from(vec![
        Span::styled("  Created   ", Style::default().fg(Color::DarkGray)),
        Span::raw(session.created_at.format("%H:%M:%S").to_string()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Active    ", Style::default().fg(Color::DarkGray)),
        Span::raw(session.last_active.format("%H:%M:%S").to_string()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Requests  ", Style::default().fg(Color::DarkGray)),
        Span::raw(session.request_count.to_string()),
    ]));

    // Separator
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ── Requests ──",
        Style::default().fg(Color::DarkGray),
    )));

    // Filter log entries for this session
    let session_entries: Vec<&super::state::LogEntry> = s
        .log_entries
        .iter()
        .filter(|e| e.session_id.as_deref() == Some(&session.id))
        .collect();

    if session_entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no requests logged yet)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for entry in &session_entries {
            let status_color = if entry.status < 300 {
                Color::Green
            } else if entry.status < 400 {
                Color::Yellow
            } else {
                Color::Red
            };

            let method_color = match entry.method.as_str() {
                "POST" => Color::Cyan,
                "GET" => Color::Green,
                "DELETE" => Color::Red,
                _ => Color::White,
            };

            let mut spans = vec![
                Span::styled(
                    format!("  {} ", entry.timestamp),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<5}", entry.method),
                    Style::default()
                        .fg(method_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<4}", entry.status),
                    Style::default().fg(status_color),
                ),
            ];

            // Duration
            if let Some(total) = entry.duration_ms {
                spans.push(Span::styled(
                    format!("{:>5} ", format_duration(total)),
                    Style::default()
                        .fg(duration_color(total))
                        .add_modifier(Modifier::BOLD),
                ));
            }

            // Label
            if let Some(ref mcp) = entry.mcp_method {
                spans.push(Span::styled(
                    mcp.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::raw(entry.path.clone()));
            }

            if !entry.note.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", entry.note),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            lines.push(Line::from(spans));
        }
    }

    let total = lines.len();
    let scroll = if total > visible_height {
        let max_scroll = total.saturating_sub(visible_height) as u16;
        s.session_detail_scroll.min(max_scroll)
    } else {
        0
    };

    let paragraph = Paragraph::new(lines).block(block).scroll((scroll, 0));
    frame.render_widget(paragraph, area);

    // Scrollbar for detail
    if total > visible_height {
        let mut scrollbar_state =
            ScrollbarState::new(total.saturating_sub(visible_height)).position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn duration_color(ms: u64) -> Color {
    if ms < 100 {
        Color::Green
    } else if ms < 500 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Word-wrap text to fit within `max_width` characters, breaking at word boundaries.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut current_line = String::new();

    for word in text.split_whitespace() {
        if current_line.is_empty() {
            current_line = word.to_string();
        } else if current_line.len() + 1 + word.len() <= max_width {
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            result.push(current_line);
            current_line = word.to_string();
        }
    }
    if !current_line.is_empty() {
        result.push(current_line);
    }
    result
}
