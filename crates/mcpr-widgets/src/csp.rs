/// CSP rewriting mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CspMode {
    /// Keep external domains from upstream, strip localhost, add configured extras + tunnel domain
    #[default]
    Extend,
    /// Ignore upstream CSP entirely, use only configured domains + tunnel domain
    Override,
}

impl std::fmt::Display for CspMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CspMode::Extend => write!(f, "extend"),
            CspMode::Override => write!(f, "override"),
        }
    }
}

pub fn parse_csp_mode(s: &str) -> CspMode {
    match s.to_lowercase().as_str() {
        "override" => CspMode::Override,
        _ => CspMode::Extend,
    }
}
