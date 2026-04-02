use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
pub struct TunnelRequest {
    pub id: String,
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>, // base64
}

#[derive(Serialize, Deserialize)]
pub struct TunnelResponse {
    pub id: String,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Option<String>, // base64
}

#[derive(Serialize, Deserialize)]
pub struct RegisterRequest {
    pub token: String,
    #[serde(default)]
    pub subdomain: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct RegisterAck {
    pub subdomain: String,
    pub url: String,
}

/// Sent by relay when client didn't specify a subdomain and auth returned allowed list.
#[derive(Serialize, Deserialize)]
pub struct SubdomainOffer {
    pub subdomains: Vec<String>,
}

/// Sent by client to pick a subdomain from the offered list.
#[derive(Serialize, Deserialize)]
pub struct SubdomainPick {
    pub subdomain: String,
}
