use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use mcpr_session::MemorySessionStore;

use super::state::{SharedTuiState, Tab};
use super::ui;

/// Install a panic hook that restores the terminal before printing the panic.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

/// Run the TUI event/render loop. Blocks until the user presses Ctrl+C, q,
/// or the shutdown signal fires.
pub fn run(
    state: SharedTuiState,
    sessions: MemorySessionStore,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    install_panic_hook();

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    loop {
        // Check if shutdown was signaled
        if *shutdown.borrow() {
            break;
        }

        terminal.draw(|f| ui::render(f, &state, &sessions))?;

        // Poll with 100ms timeout → ~10 FPS refresh
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            // Ignore key release events (Windows/crossterm quirk)
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Tab | KeyCode::BackTab => {
                    let mut s = state.lock().unwrap();
                    s.active_tab = match s.active_tab {
                        Tab::Requests => Tab::Sessions,
                        Tab::Sessions => Tab::Requests,
                    };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let mut s = state.lock().unwrap();
                    match s.active_tab {
                        Tab::Requests => {
                            s.auto_scroll = false;
                            let max = s.log_entries.len().saturating_sub(1) as u16;
                            s.scroll_offset = (s.scroll_offset + 1).min(max);
                        }
                        Tab::Sessions => {
                            let count = sessions.list_sync().len();
                            if count > 0 {
                                s.selected_session = (s.selected_session + 1).min(count - 1);
                                s.session_detail_scroll = 0;
                            }
                        }
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let mut s = state.lock().unwrap();
                    match s.active_tab {
                        Tab::Requests => {
                            s.auto_scroll = false;
                            s.scroll_offset = s.scroll_offset.saturating_sub(1);
                        }
                        Tab::Sessions => {
                            s.selected_session = s.selected_session.saturating_sub(1);
                            s.session_detail_scroll = 0;
                        }
                    }
                }
                KeyCode::Home => {
                    let mut s = state.lock().unwrap();
                    match s.active_tab {
                        Tab::Requests => {
                            s.auto_scroll = false;
                            s.scroll_offset = 0;
                        }
                        Tab::Sessions => {
                            s.selected_session = 0;
                            s.session_detail_scroll = 0;
                        }
                    }
                }
                KeyCode::End | KeyCode::Char('G') => {
                    let mut s = state.lock().unwrap();
                    match s.active_tab {
                        Tab::Requests => {
                            s.auto_scroll = true;
                        }
                        Tab::Sessions => {
                            let count = sessions.list_sync().len();
                            if count > 0 {
                                s.selected_session = count - 1;
                            }
                            s.session_detail_scroll = 0;
                        }
                    }
                }
                KeyCode::PageDown => {
                    let mut s = state.lock().unwrap();
                    match s.active_tab {
                        Tab::Requests => {
                            s.auto_scroll = false;
                            let max = s.log_entries.len().saturating_sub(1) as u16;
                            s.scroll_offset = (s.scroll_offset + 20).min(max);
                        }
                        Tab::Sessions => {
                            s.session_detail_scroll = s.session_detail_scroll.saturating_add(10);
                        }
                    }
                }
                KeyCode::PageUp => {
                    let mut s = state.lock().unwrap();
                    match s.active_tab {
                        Tab::Requests => {
                            s.auto_scroll = false;
                            s.scroll_offset = s.scroll_offset.saturating_sub(20);
                        }
                        Tab::Sessions => {
                            s.session_detail_scroll = s.session_detail_scroll.saturating_sub(10);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Restore terminal
    terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}
