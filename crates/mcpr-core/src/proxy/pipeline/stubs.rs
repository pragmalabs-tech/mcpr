//! Small shared types referenced by `values.rs`.
//!
//! `SessionId` and `TagSet` are load-bearing — middlewares construct
//! and mutate them through the pipeline. `UrlMap` and `OAuthKind` exist
//! so `Request::OAuth` / `Route::Oauth` have inhabited argument types;
//! intake does not yet produce OAuth classifications.

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

#[derive(Debug, Clone, Default)]
pub struct UrlMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthKind {
    Discovery,
    Token,
    Callback,
    Unknown,
}

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
