# Deploying mcpr Relay Server

## Prerequisites

- VPS with public IP (Ubuntu 22.04+ recommended)
- Domain with DNS control (Cloudflare, Route53, etc.)
- Wildcard DNS support
- Docker installed on VPS (optional)

## 1. DNS

Add a wildcard A record pointing to your VPS. Example using Cloudflare:

| Type | Name | Content | Proxy status |
|------|------|---------|--------------|
| A | `tunnel` | `YOUR_VPS_IP` | DNS only (grey cloud) |
| A | `*.tunnel` | `YOUR_VPS_IP` | DNS only (grey cloud) |

**Important**: You need both records — the wildcard doesn't match the bare domain. If using Cloudflare, use "DNS only" (grey cloud), not "Proxied" — Cloudflare's proxy doesn't handle arbitrary WebSocket upgrades well.

## 2. TLS Certificate

You need a wildcard certificate for `*.tunnel.yourdomain.com`. Example with certbot + Cloudflare DNS:

```bash
apt update && apt install -y nginx certbot python3-certbot-dns-cloudflare

# Create credentials file
cat > /etc/cloudflare.ini << 'EOF'
dns_cloudflare_api_token = YOUR_CF_API_TOKEN
EOF
chmod 600 /etc/cloudflare.ini

# Obtain wildcard cert
certbot certonly \
  --dns-cloudflare \
  --dns-cloudflare-credentials /etc/cloudflare.ini \
  -d "*.tunnel.yourdomain.com" \
  -d "tunnel.yourdomain.com" \
  --agree-tos \
  -m you@example.com
```

Certbot auto-renews via systemd timer. Verify: `systemctl list-timers | grep certbot`

Other DNS providers: use the appropriate certbot DNS plugin or use Caddy for automatic TLS.

## 3. Reverse Proxy

### nginx

Create `/etc/nginx/conf.d/tunnel.conf`:

```nginx
server {
    listen 443 ssl;
    server_name tunnel.yourdomain.com *.tunnel.yourdomain.com;

    ssl_certificate /etc/letsencrypt/live/tunnel.yourdomain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/tunnel.yourdomain.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 86400s;
        proxy_send_timeout 86400s;
    }
}

server {
    listen 80;
    server_name tunnel.yourdomain.com *.tunnel.yourdomain.com;
    return 301 https://$host$request_uri;
}
```

```bash
nginx -t && systemctl reload nginx
```

### Caddy (alternative)

```
*.tunnel.yourdomain.com {
    reverse_proxy localhost:8080
}
```

Caddy handles TLS automatically with wildcard certs via DNS challenge.

## 4. Configuration

Create `mcpr.toml` on the relay server:

### Open mode (no auth -- anyone can tunnel)

```toml
mode = "relay"
port = 8080

[relay]
domain = "tunnel.yourdomain.com"
```

### Static tokens (simple -- no external service)

```toml
mode = "relay"
port = 8080

[relay]
domain = "tunnel.yourdomain.com"

[[relay.tokens]]
token = "mcpr_abc123"
subdomains = ["myapp", "myapp-*"]

[[relay.tokens]]
token = "mcpr_def456"
subdomains = ["other-app", "other-app-*"]
```

Each token has a list of allowed subdomain patterns. Patterns support glob wildcards:
`myapp-*`, `*-preview`, `pr-*-acme`, `*` (allow all).

The client sets `[tunnel].token` in their `mcpr.toml` to one of these values.
See [STATIC_TOKENS.md](STATIC_TOKENS.md) for real-world scenarios (team setup, CI/CD previews, demos).

### Secured mode (with auth provider)

```toml
mode = "relay"
port = 8080

[relay]
domain = "tunnel.yourdomain.com"
auth_provider = "https://auth.yourdomain.com"
auth_provider_secret = "your-shared-secret-here"
```

When `auth_provider` is set, every tunnel registration is validated via:

```
POST {auth_provider}/api/verify
Header: X-Relay-Secret: {auth_provider_secret}
Body:   { "token": "...", "subdomain": "..." }

200 -> { "subdomains": ["myapp", "myapp-*"] }   (allowed)
401 -> { "error": "invalid_token" }              (rejected)
403 -> { "error": "subdomain_not_allowed" }      (rejected)
```

Subdomain patterns support wildcards: `myapp-*` matches `myapp-dev`, `myapp-feat-123`, etc.

The relay itself has no database -- it delegates all auth decisions to your provider.

## 5. Run the Relay

### Direct

```bash
# With mcpr.toml in current directory:
mcpr start --foreground --relay

# Or with CLI flags (no config file needed):
mcpr start --foreground --relay --port 8080 --relay-domain tunnel.yourdomain.com
```

### With Docker

```bash
docker run -d \
  --name mcpr-relay \
  --restart unless-stopped \
  -p 8080:8080 \
  ghcr.io/cptrodgers/mcpr:latest \
  --relay --port 8080 --relay-domain tunnel.yourdomain.com
```

To enable auth via Docker, pass environment variables:

```bash
docker run -d \
  --name mcpr-relay \
  --restart unless-stopped \
  -p 8080:8080 \
  -e MCPR_AUTH_PROVIDER=https://auth.yourdomain.com \
  -e MCPR_AUTH_PROVIDER_SECRET=your-shared-secret-here \
  ghcr.io/cptrodgers/mcpr:latest \
  --relay --port 8080 --relay-domain tunnel.yourdomain.com
```

### Update

```bash
docker pull ghcr.io/cptrodgers/mcpr:latest
docker stop mcpr-relay && docker rm mcpr-relay
# re-run the docker run command above
```

## 6. Client Setup

On the developer's machine, create `mcpr.toml`:

```toml
mcp = "http://localhost:9000/mcp"
widgets = "http://localhost:4444"

[tunnel]
relay_url = "https://tunnel.yourdomain.com"
# subdomain = "myapp"          # optional fixed subdomain
# token = "your-api-token"     # required when relay has auth enabled
```

Then run:

```bash
mcpr
# Should print: Tunnel: https://xxxxxx.tunnel.yourdomain.com
```

## 7. Verify

```bash
# Check relay is reachable
curl -s https://tunnel.yourdomain.com/_tunnel/register
# Expected: "missing token" (400) -- means relay is running

# Full test: start mcpr client
mcpr start --mcp http://localhost:9000 --relay-url https://tunnel.yourdomain.com
```

## Troubleshooting

| Issue | Fix |
|-------|-----|
| `tunnel not found` | Client not connected. Check mcpr client logs. |
| `invalid token` / 401 | Auth provider rejected the token. Check your `[tunnel].token`. |
| `subdomain not authorized` / 403 | Token valid but not allowed for this subdomain. |
| `auth provider unavailable` / 503 | Relay can't reach the auth provider. Check URL and network. |
| SSL errors | Check cert: `sudo certbot certificates` |
| WebSocket timeout | Verify nginx `proxy_read_timeout` is set high |
| 502 from nginx | Check relay is running: `docker logs mcpr-relay` |
