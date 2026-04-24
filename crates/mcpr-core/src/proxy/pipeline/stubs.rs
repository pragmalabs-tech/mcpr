//! Placeholder types referenced by `values.rs`. Real owners arrive in
//! later refactor phases — each item carries the phase it moves to,
//! so the cleanup grep is `rg '// phase-[0-9]+' crates`.

use std::time::Instant;

// phase-3: replaced by subsystems::session::SessionId (newtype on String).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// phase-3: replaced by subsystems::session::SessionRecord.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: SessionId,
    pub created_at: Instant,
}

// phase-5: replaced by transport::UrlMap. For now `Route::Oauth` carries
// it as an empty marker; Phase 5 fills in the upstream→proxy URL
// rewrite table.
#[derive(Debug, Clone, Default)]
pub struct UrlMap;

// phase-5: replaced by intake::OAuthKind. Content-based classification
// lands with intake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthKind {
    Discovery,
    Token,
    Callback,
    Unknown,
}

// phase-4: today `pipeline/context.rs` uses `Vec<&'static str>` directly
// and `pipeline/emit.rs` joins with `+`. The `TagSet` newtype exists so
// the architecture-doc terminology survives into the target pipeline;
// Phase 4 can swap the backing store if a richer tag API is needed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagSet(pub Vec<&'static str>);

impl TagSet {
    pub fn push(&mut self, tag: &'static str) {
        self.0.push(tag);
    }

    pub fn as_slice(&self) -> &[&'static str] {
        &self.0
    }
}
