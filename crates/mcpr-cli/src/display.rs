use mcpr_proxy::state::SharedProxyState;

/// Populate the proxy state with startup info and print to stderr.
pub fn log_startup(
    state: &SharedProxyState,
    port: u16,
    public_url: &str,
    mcp_upstream: &str,
    widgets: Option<&str>,
) {
    let mut s = mcpr_proxy::lock_state(state);
    s.proxy_url = format!("http://localhost:{port}");
    s.tunnel_url = public_url.to_string();
    s.mcp_upstream = mcp_upstream.to_string();
    s.widgets = widgets.unwrap_or("(none)").to_string();
}
