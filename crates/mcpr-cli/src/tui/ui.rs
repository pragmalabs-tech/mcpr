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

    // Layout: top info bar + bottom content (tabs + requests/sessions)
    let info_height = compute_info_height(&s);
    let chunks = Layout::vertical([Constraint::Length(info_height), Constraint::Min(10)])
        .split(frame.area());

    render_info_panel(frame, chunks[0], &s);

    // Bottom panel: tab bar + content
    let bottom_chunks =
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
    frame.render_widget(tabs, bottom_chunks[0]);

    match s.active_tab {
        Tab::Requests => render_log_panel(frame, bottom_chunks[1], &s),
        Tab::Sessions => render_sessions_panel(frame, bottom_chunks[1], sessions, &s),
    }
}

/// Calculate how many rows the info panel needs.
fn compute_info_height(s: &super::state::TuiState) -> u16 {
    let mut rows: u16 = 5; // border(2) + urls line + status line + studio line
    if s.tunnel_anonymous {
        rows += 1; // temp warning
    }
    if s.mcp_warning.is_some() {
        rows += 1;
    }
    if s.cloud_endpoint.is_some() {
        rows += 1;
    }
    if !s.widget_names.is_empty() {
        rows += 1; // widgets found line
    }
    rows
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

fn status_icon(status: ConnectionStatus) -> Span<'static> {
    let (color, symbol) = status_style(status);
    Span::styled(symbol.to_string(), Style::default().fg(color))
}

fn render_info_panel(frame: &mut Frame, area: Rect, s: &super::state::TuiState) {
    let block = Block::default()
        .title(" mcpr proxy ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner_width = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Row 1: URLs — Tunnel | Proxy | MCP | Widgets
    let mut url_spans: Vec<Span> = Vec::new();

    if !s.tunnel_url.is_empty() {
        url_spans.push(Span::styled(
            " Tunnel ",
            Style::default().fg(Color::DarkGray),
        ));
        url_spans.push(status_icon(s.tunnel_status));
        url_spans.push(Span::raw(" "));
        url_spans.push(Span::styled(
            s.tunnel_url.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        if s.tunnel_anonymous {
            url_spans.push(Span::styled(" (temp)", Style::default().fg(Color::Yellow)));
        }
        url_spans.push(Span::styled("  │", Style::default().fg(Color::DarkGray)));
    }

    url_spans.push(Span::styled(
        " Proxy ",
        Style::default().fg(Color::DarkGray),
    ));
    url_spans.push(Span::raw(s.proxy_url.clone()));

    url_spans.push(Span::styled(
        "  │ MCP ",
        Style::default().fg(Color::DarkGray),
    ));
    url_spans.push(status_icon(s.mcp_status));
    url_spans.push(Span::raw(format!(" {}", s.mcp_upstream)));

    if !s.widgets.is_empty() {
        url_spans.push(Span::styled(
            "  │ Widgets ",
            Style::default().fg(Color::DarkGray),
        ));
        url_spans.push(status_icon(s.widgets_status));
        if let Some(count) = s.widget_count {
            url_spans.push(Span::raw(format!(" {} ({count})", s.widgets)));
        } else {
            url_spans.push(Span::raw(format!(" {}", s.widgets)));
        }
    }

    url_spans.push(Span::styled(
        "  │ Reqs ",
        Style::default().fg(Color::DarkGray),
    ));
    url_spans.push(Span::raw(s.request_count.to_string()));

    url_spans.push(Span::styled(
        "  │ Up ",
        Style::default().fg(Color::DarkGray),
    ));
    url_spans.push(Span::raw(s.uptime()));

    lines.push(Line::from(url_spans));

    // Row 2: MCP warning (if any)
    if let Some(ref warning) = s.mcp_warning {
        let max_warn = inner_width.saturating_sub(4);
        let warn_text = if warning.len() > max_warn {
            format!("{}…", &warning[..max_warn.saturating_sub(1)])
        } else {
            warning.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(" ⚠ ", Style::default().fg(Color::Yellow)),
            Span::styled(warn_text, Style::default().fg(Color::Yellow)),
        ]));
    }

    // Cloud sync status
    if let Some(ref endpoint) = s.cloud_endpoint {
        let mut spans = vec![
            Span::styled(" Cloud ", Style::default().fg(Color::DarkGray)),
            Span::styled(endpoint.as_str(), Style::default().fg(Color::Cyan)),
        ];
        if let Some(ref sync) = s.cloud_sync {
            match sync {
                super::state::CloudSyncStatus::Ok { count } => {
                    spans.push(Span::styled(
                        format!("  synced {count}"),
                        Style::default().fg(Color::Green),
                    ));
                }
                super::state::CloudSyncStatus::Failed { message } => {
                    let max_len = inner_width.saturating_sub(endpoint.len() + 12);
                    let text = if message.len() > max_len {
                        format!("{}…", &message[..max_len.saturating_sub(1)])
                    } else {
                        message.clone()
                    };
                    spans.push(Span::styled(
                        format!("  {text}"),
                        Style::default().fg(Color::Red),
                    ));
                }
            }
        }
        lines.push(Line::from(spans));
    }

    // Row 3: Studio URL + widgets + shortcuts
    let proxy = if !s.tunnel_url.is_empty() {
        &s.tunnel_url
    } else {
        &s.proxy_url
    };
    let studio_url = format!("https://cloud.mcpr.app/studio?proxy={}", proxy);

    let mut bottom_spans = vec![
        Span::styled(" Studio ", Style::default().fg(Color::DarkGray)),
        Span::styled(studio_url, Style::default().fg(Color::Cyan)),
    ];

    if !s.widget_names.is_empty() {
        let names = s.widget_names.join(", ");
        bottom_spans.push(Span::styled(
            "  │ Widgets: ",
            Style::default().fg(Color::DarkGray),
        ));
        bottom_spans.push(Span::raw(names));
    }

    lines.push(Line::from(bottom_spans));

    // Row 4: Temp tunnel warning
    if s.tunnel_anonymous {
        lines.push(Line::from(vec![Span::styled(
            " ⚠ Temporary tunnel — expires in 1 week. Provide email to keep your subdomain.",
            Style::default().fg(Color::Yellow),
        )]));
    }

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

            // Size (req→resp)
            let req_str = match entry.req_size {
                Some(size) => format_bytes(size),
                None => "-".to_string(),
            };
            let resp_str = match entry.resp_size {
                Some(size) => format_bytes(size),
                None => "-".to_string(),
            };
            spans.push(Span::styled(
                format!("{:>5}→{:<5} ", req_str, resp_str),
                Style::default().fg(Color::DarkGray),
            ));

            // Duration
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

            // Label
            if let Some(ref mcp) = entry.mcp_method {
                spans.push(Span::styled(
                    mcp.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
                if let Some(ref detail) = entry.detail {
                    spans.push(Span::styled(
                        format!(" {detail}"),
                        Style::default().fg(Color::White),
                    ));
                }
            } else {
                spans.push(Span::raw(entry.path.clone()));
            }

            if let Some((code, ref msg)) = entry.jsonrpc_error {
                spans.push(Span::styled(
                    format!(" [{code} {msg}]"),
                    Style::default().fg(Color::Red),
                ));
            }

            if let Some(ref url) = entry.upstream_url {
                spans.push(Span::styled(
                    format!(" → {url}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }

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

    let chunks = Layout::vertical([
        Constraint::Length(session_list.len() as u16 + 2),
        Constraint::Min(6),
    ])
    .split(area);

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

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ── Requests ──",
        Style::default().fg(Color::DarkGray),
    )));

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

            if let Some(total) = entry.duration_ms {
                spans.push(Span::styled(
                    format!("{:>5} ", format_duration(total)),
                    Style::default()
                        .fg(duration_color(total))
                        .add_modifier(Modifier::BOLD),
                ));
            }

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
