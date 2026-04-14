use mcpr_core::proxy::state::SharedProxyState;

/// Populate the proxy state with startup info and print a startup banner to stderr.
pub fn log_startup(
    state: &SharedProxyState,
    port: u16,
    public_url: &str,
    mcp_upstream: &str,
    widgets: Option<&str>,
) {
    let mut s = mcpr_core::proxy::lock_state(state);
    s.proxy_url = format!("http://localhost:{port}");
    s.tunnel_url = public_url.to_string();
    s.mcp_upstream = mcp_upstream.to_string();
    s.widgets = widgets.unwrap_or("(none)").to_string();
    drop(s);

    eprintln!();
    eprintln!("  {} mcpr proxy running", colored::Colorize::green("ready"),);
    eprintln!("  proxy:    http://localhost:{port}");
    if public_url != format!("http://localhost:{port}") {
        eprintln!("  tunnel:   {public_url}");
    }
    eprintln!("  upstream: {mcp_upstream}");
    if let Some(w) = widgets {
        eprintln!("  widgets:  {w}");
    }
    eprintln!();
}
