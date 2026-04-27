# Static Token Authentication

The simplest way to secure your relay -- no external service, no database, just tokens in `mcpr.toml`.

## When to use

- Small team (2-10 developers) sharing a relay
- CI/CD pipelines that need stable preview URLs
- Personal relay you want to lock down
- Quick setup before deciding on a full auth provider

## Setup

### 1. Generate tokens

```bash
# Generate a random token
openssl rand -hex 32
# → a1b2c3d4e5f6...

# Or use a prefix for easy identification
echo "mcpr_$(openssl rand -hex 24)"
# → mcpr_a1b2c3d4e5f6...
```

### 2. Configure the relay

```toml
# mcpr.toml (on relay server)
mode = "relay"
port = 8081

[relay]
domain = "tunnel.yourdomain.com"

[[relay.tokens]]
token = "mcpr_alice_a1b2c3d4e5f6"
subdomains = ["alice-*"]

[[relay.tokens]]
token = "mcpr_bob_f6e5d4c3b2a1"
subdomains = ["bob-*"]
```

### 3. Give each developer their token

Developer adds it to their local `mcpr.toml`:

```toml
mcp = "http://localhost:9000/mcp"

[tunnel]
relay_url = "https://tunnel.yourdomain.com"
token = "mcpr_alice_a1b2c3d4e5f6"
subdomain = "alice-myapp"
```

```bash
mcpr
# → https://alice-myapp.tunnel.yourdomain.com
```

## Common Scenarios

### One developer, multiple projects

```toml
[[relay.tokens]]
token = "mcpr_rodgers_abc123"
subdomains = ["rodgers-*"]
```

The developer can use any subdomain starting with `rodgers-`:

```
rodgers-webapp.tunnel.yourdomain.com
rodgers-api.tunnel.yourdomain.com
rodgers-experiment.tunnel.yourdomain.com
```

### CI/CD branch previews

Give your CI pipeline a token that allows PR-based subdomains:

```toml
[[relay.tokens]]
token = "mcpr_ci_pipeline_xyz789"
subdomains = ["pr-*", "staging"]
```

In your CI workflow (GitHub Actions example):

```yaml
- name: Deploy preview
  run: |
    # mcpr.toml: mcp = "http://localhost:9000", [tunnel] relay_url = "...", token + subdomain from env
    mcpr proxy run mcpr.toml
  env:
    # Set in GitHub Secrets
    MCPR_TUNNEL_TOKEN: ${{ secrets.MCPR_CI_TOKEN }}
    MCPR_TUNNEL_SUBDOMAIN: pr-${{ github.event.pull_request.number }}
```

Each PR gets its own URL: `pr-42.tunnel.yourdomain.com`, `pr-123.tunnel.yourdomain.com`.

### Shared team relay with project isolation

```toml
[[relay.tokens]]
token = "mcpr_frontend_team_aaa"
subdomains = ["web-*", "ui-*"]

[[relay.tokens]]
token = "mcpr_backend_team_bbb"
subdomains = ["api-*", "svc-*"]

[[relay.tokens]]
token = "mcpr_qa_team_ccc"
subdomains = ["qa-*", "test-*"]
```

Frontend can't squat on `api-*` subdomains and vice versa.

### Demo/client presentations

Create a short-lived token for a demo with a clean subdomain:

```toml
[[relay.tokens]]
token = "mcpr_demo_acme_2026q2"
subdomains = ["demo-acme"]
```

Gives you a stable `demo-acme.tunnel.yourdomain.com` for the demo. Remove the entry when done.

### Personal relay lockdown

Just one token, allow everything:

```toml
[[relay.tokens]]
token = "mcpr_myrelay_secrettoken123"
subdomains = ["*"]
```

Only you can tunnel, but any subdomain works.

## Revoking access

Remove or comment out the token entry and restart the relay:

```toml
# [[relay.tokens]]
# token = "mcpr_bob_f6e5d4c3b2a1"
# subdomains = ["bob-*"]
```

Active tunnels from that token will continue until they disconnect. New connections will be rejected immediately.

## Tips

- **Name your tokens** -- use prefixes like `mcpr_alice_`, `mcpr_ci_`, `mcpr_demo_` so you know who/what each token belongs to.
- **One token per person/service** -- easier to revoke without affecting others.
- **Use narrow patterns** -- `alice-*` is better than `*`. Prevents accidental subdomain collisions between developers.
- **Keep the config file secure** -- tokens are stored in plaintext. Set `chmod 600 mcpr.toml` on the relay server.

## Upgrading to an auth provider

When static tokens become hard to manage (many developers, frequent rotation, self-service), switch to an [auth provider](AUTH_PROVIDER.md). The client-side config stays the same -- only the relay config changes.
