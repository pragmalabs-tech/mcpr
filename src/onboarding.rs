//! Onboarding flow for tunnel.mcpr.app — claim a subdomain via the API.

use serde::{Deserialize, Serialize};

const API_BASE: &str = "https://api.mcpr.app";

#[derive(Serialize)]
struct ClaimRequest {
    subdomain: String,
    email: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ClaimResponse {
    subdomain: String,
    token: String,
    status: String,
    expires_at: Option<u64>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct AnonymousResponse {
    subdomain: String,
    token: String,
    status: String,
    expires_at: Option<u64>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct CheckResponse {
    available: bool,
    name: String,
}

#[derive(Deserialize)]
struct ApiError {
    error: Option<String>,
    message: Option<String>,
}

/// Result of the onboarding flow.
pub struct OnboardingResult {
    pub token: String,
    pub subdomain: String,
    /// Whether this was an anonymous (no-email) claim.
    pub anonymous: bool,
}

type ClaimFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<OnboardingResult, String>> + Send + 'a>,
>;

/// Run the interactive onboarding flow for tunnel.mcpr.app.
/// Returns OnboardingResult on success.
pub fn run_claim_flow(existing_subdomain: Option<&str>) -> ClaimFuture<'_> {
    Box::pin(run_claim_flow_inner(existing_subdomain))
}

async fn run_claim_flow_inner(
    existing_subdomain: Option<&str>,
) -> Result<OnboardingResult, String> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("Failed to build onboarding HTTP client");

    // If no existing subdomain, offer anonymous quick-start
    if existing_subdomain.is_none() {
        eprintln!(
            "  {} Try instantly without an account, or claim a custom subdomain.",
            colored::Colorize::cyan("?"),
        );
        eprint!("  Try instantly? [Y/n]: ");
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {e}"))?;
        let input = input.trim().to_lowercase();

        if input.is_empty() || input == "y" || input == "yes" {
            return run_anonymous_flow(&http).await;
        }
        // User chose 'n' — fall through to the claim flow
        eprintln!();
    }

    run_email_claim_flow(&http, existing_subdomain).await
}

/// Anonymous flow: no email, instant random subdomain.
async fn run_anonymous_flow(http: &reqwest::Client) -> Result<OnboardingResult, String> {
    eprintln!("  Creating anonymous tunnel...");
    let resp = http
        .post(format!("{API_BASE}/api/subdomains/anonymous"))
        .send()
        .await
        .map_err(|e| format!("Failed to reach API: {e}"))?;

    let status = resp.status();
    if status.is_success() {
        let claim: AnonymousResponse = resp
            .json()
            .await
            .map_err(|e| format!("Invalid API response: {e}"))?;
        eprintln!(
            "  {} tunnel ready at '{}.tunnel.mcpr.app' (expires in 1 week)",
            colored::Colorize::green("✓"),
            claim.subdomain,
        );
        Ok(OnboardingResult {
            token: claim.token,
            subdomain: claim.subdomain,
            anonymous: true,
        })
    } else {
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<ApiError>(&body)
            .ok()
            .and_then(|e| e.message.or(e.error))
            .unwrap_or(body);
        Err(format!("Anonymous claim failed ({status}): {msg}"))
    }
}

/// Email-based claim flow: pick a subdomain, provide email, get 72h reservation.
async fn run_email_claim_flow(
    http: &reqwest::Client,
    existing_subdomain: Option<&str>,
) -> Result<OnboardingResult, String> {
    // 1. Get subdomain
    let subdomain = match existing_subdomain {
        Some(s) => {
            eprintln!("  Using subdomain from config: {s}");
            s.to_string()
        }
        None => ask_subdomain(http).await?,
    };

    // Verify the subdomain is available (even if from config)
    if existing_subdomain.is_some() {
        let available = check_subdomain(http, &subdomain).await?;
        if !available {
            eprintln!(
                "  {} subdomain '{}' is not available",
                colored::Colorize::yellow("warn"),
                subdomain
            );
            // Fall back to interactive selection
            return run_claim_flow(None).await;
        }
    }

    // 2. Ask email
    let email = ask_email()?;

    // 3. Claim subdomain
    eprintln!("  Claiming subdomain '{subdomain}'...");
    let resp = http
        .post(format!("{API_BASE}/api/subdomains/claim"))
        .json(&ClaimRequest {
            subdomain: subdomain.clone(),
            email,
        })
        .send()
        .await
        .map_err(|e| format!("Failed to reach API: {e}"))?;

    let status = resp.status();
    if status.is_success() {
        let claim: ClaimResponse = resp
            .json()
            .await
            .map_err(|e| format!("Invalid API response: {e}"))?;
        eprintln!(
            "  {} subdomain '{}' claimed (status: {})",
            colored::Colorize::green("✓"),
            claim.subdomain,
            claim.status,
        );
        Ok(OnboardingResult {
            token: claim.token,
            subdomain: claim.subdomain,
            anonymous: false,
        })
    } else if status.as_u16() == 409 {
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<ApiError>(&body)
            .ok()
            .and_then(|e| e.message.or(e.error))
            .unwrap_or_else(|| "Subdomain taken".into());
        eprintln!("  {} {msg}", colored::Colorize::yellow("conflict"),);
        eprintln!("  Pick a different subdomain.\n");
        // Retry with fresh subdomain
        run_claim_flow(None).await
    } else {
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<ApiError>(&body)
            .ok()
            .and_then(|e| e.message.or(e.error))
            .unwrap_or(body);
        Err(format!("Claim failed ({status}): {msg}"))
    }
}

/// Interactively ask for a subdomain, checking availability.
async fn ask_subdomain(http: &reqwest::Client) -> Result<String, String> {
    loop {
        eprint!("  Choose a subdomain (e.g. my-app): ");
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {e}"))?;
        let input = input.trim().to_lowercase();

        if input.is_empty() {
            eprintln!("  Subdomain cannot be empty.");
            continue;
        }

        if !is_valid_subdomain(&input) {
            eprintln!("  Must be 3-63 chars: lowercase letters, numbers, and hyphens only.");
            continue;
        }

        let available = check_subdomain(http, &input).await?;
        if available {
            eprintln!(
                "  {} '{input}' is available!",
                colored::Colorize::green("✓"),
            );
            return Ok(input);
        } else {
            eprintln!(
                "  {} '{input}' is taken. Try another.",
                colored::Colorize::yellow("✗"),
            );
        }
    }
}

/// Check subdomain availability via the API.
async fn check_subdomain(http: &reqwest::Client, name: &str) -> Result<bool, String> {
    let resp = http
        .get(format!("{API_BASE}/api/subdomains/check/{name}"))
        .send()
        .await
        .map_err(|e| format!("Failed to check subdomain: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Subdomain check failed ({})", resp.status()));
    }

    let check: CheckResponse = resp
        .json()
        .await
        .map_err(|e| format!("Invalid check response: {e}"))?;
    Ok(check.available)
}

/// Ask for an email address.
fn ask_email() -> Result<String, String> {
    eprintln!("  We'll send a verification link to claim your subdomain.");
    loop {
        eprint!("  Email: ");
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {e}"))?;
        let input = input.trim().to_string();

        if input.contains('@') && input.contains('.') {
            return Ok(input);
        }
        eprintln!("  That doesn't look right. Please enter a valid email.");
    }
}

/// Basic subdomain validation.
fn is_valid_subdomain(s: &str) -> bool {
    let len = s.len();
    (3..=63).contains(&len)
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.ends_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}
