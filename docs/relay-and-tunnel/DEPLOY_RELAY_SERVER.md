# Deploying mcpr Relay Server

## Prerequisites

- VPS with public IP (Ubuntu 22.04+ recommended)
- Domain with DNS control (Cloudflare, Route53, etc.)
- Wildcard DNS support
- Docker installed on VPS

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

Create `mcpr.toml` on the relay server. The relay supports three auth modes — pick one.

### Open mode (no auth)

Anyone can tunnel. Good for local dev and testing.

```toml
mode = "relay"
port = 8080

[relay]
domain = "tunnel.yourdomain.com"
```

### Static tokens

No external service, no database — just tokens in `mcpr.toml`. Good for small teams (2-10 developers), CI/CD pipelines, and personal relays.

```toml
mode = "relay"
port = 8080

[relay]
domain = "tunnel.yourdomain.com"

[[relay.tokens]]
token = "mcpr_alice_a1b2c3d4e5f6"
subdomains = ["alice-*"]

[[relay.tokens]]
token = "mcpr_bob_f6e5d4c3b2a1"
subdomains = ["bob-*"]

[[relay.tokens]]
token = "mcpr_ci_pipeline_xyz789"
subdomains = ["pr-*", "staging"]
```

Each token has a list of allowed subdomain patterns. The client sets `[tunnel].token` in their `mcpr.toml` to one of these values.

**Generate tokens:**

```bash
echo "mcpr_$(openssl rand -hex 24)"
# → mcpr_a1b2c3d4e5f6...
```

**Common scenarios:**

| Scenario | Token pattern | Subdomains |
|----------|--------------|------------|
| One dev, multiple projects | `mcpr_alice_...` | `["alice-*"]` |
| CI/CD branch previews | `mcpr_ci_...` | `["pr-*", "staging"]` |
| Team with project isolation | `mcpr_frontend_...` | `["web-*", "ui-*"]` |
| Demo/presentation | `mcpr_demo_acme_...` | `["demo-acme"]` |
| Personal relay lockdown | `mcpr_myrelay_...` | `["*"]` |

**Revoking access:** Remove or comment out the token entry and restart the relay. Active tunnels continue until they disconnect; new connections are rejected immediately.

**Tips:**
- Use prefixes like `mcpr_alice_`, `mcpr_ci_` so you know who each token belongs to
- One token per person/service — easier to revoke without affecting others
- Use narrow patterns (`alice-*` not `*`) to prevent subdomain collisions
- Keep the config secure: `chmod 600 mcpr.toml` on the relay server

### Auth provider

For dynamic token management, user registration, and revocation at scale. The relay delegates all auth decisions to your external API — it has no database.

```toml
mode = "relay"
port = 8080

[relay]
domain = "tunnel.yourdomain.com"
auth_provider = "https://auth.yourdomain.com"
auth_provider_secret = "your-shared-secret-here"
```

Generate a strong shared secret: `openssl rand -hex 32`

**How it works:**

```
Developer's mcpr client
    │  WS /_tunnel/register?token=TOKEN&subdomain=myapp
    ▼
mcpr relay
    │  POST /api/verify
    ▼
Your Auth Provider
    │  200: allowed subdomains
    │  401: invalid token
    │  403: subdomain not allowed
    ▼
mcpr relay → allows or rejects the tunnel
```

**API contract — your provider must implement:**

```
POST /api/verify
Header: X-Relay-Secret: <shared_secret>
Body:   { "token": "mcpr_...", "subdomain": "myapp-dev" }

200 → { "subdomains": ["myapp", "myapp-*"] }   (allowed)
401 → { "error": "invalid_token" }              (rejected)
403 → { "error": "subdomain_not_allowed" }      (rejected)
```

The `X-Relay-Secret` header verifies the request came from your relay. Your provider should reject requests without it.

**Example: Node.js + Express**

```js
import express from 'express';

const RELAY_SECRET = process.env.RELAY_SECRET || 'dev-secret';

// In production, use a database
const tokens = {
  'mcpr_test_token_123': {
    user: 'rodgers',
    subdomains: ['myapp', 'myapp-*'],
  },
};

const app = express();
app.use(express.json());

app.post('/api/verify', (req, res) => {
  if (req.headers['x-relay-secret'] !== RELAY_SECRET) {
    return res.status(401).json({ error: 'invalid relay secret' });
  }

  const { token, subdomain } = req.body;
  const entry = tokens[token];

  if (!entry) {
    return res.status(401).json({ error: 'invalid_token' });
  }

  res.json({ subdomains: entry.subdomains });
});

app.listen(3001, () => console.log('Auth provider on :3001'));
```

**Example: Cloudflare Worker + D1**

```js
export default {
  async fetch(request, env) {
    if (new URL(request.url).pathname !== '/api/verify') {
      return new Response('Not found', { status: 404 });
    }

    const relaySecret = request.headers.get('x-relay-secret');
    if (relaySecret !== env.RELAY_SECRET) {
      return Response.json({ error: 'invalid relay secret' }, { status: 401 });
    }

    const { token, subdomain } = await request.json();

    const hash = await crypto.subtle.digest(
      'SHA-256',
      new TextEncoder().encode(token)
    );
    const tokenHash = [...new Uint8Array(hash)]
      .map(b => b.toString(16).padStart(2, '0'))
      .join('');

    const row = await env.DB
      .prepare('SELECT subdomains FROM tokens WHERE token_hash = ? AND revoked_at IS NULL')
      .bind(tokenHash)
      .first();

    if (!row) {
      return Response.json({ error: 'invalid_token' }, { status: 401 });
    }

    return Response.json({ subdomains: JSON.parse(row.subdomains) });
  },
};
```

**Testing your provider:**

```bash
# Should return 200 with subdomains
curl -X POST https://auth.yourdomain.com/api/verify \
  -H "Content-Type: application/json" \
  -H "X-Relay-Secret: your-secret" \
  -d '{"token": "mcpr_test_token", "subdomain": "myapp"}'

# Should return 401 (bad token)
curl -X POST https://auth.yourdomain.com/api/verify \
  -H "Content-Type: application/json" \
  -H "X-Relay-Secret: your-secret" \
  -d '{"token": "invalid_token", "subdomain": "myapp"}'
```

**Security considerations:**
- Never store tokens in plaintext — hash with SHA-256 or bcrypt
- Always run your auth provider behind HTTPS
- Rate-limit `/api/verify` to prevent brute-force (traffic is low — one call per tunnel registration)

### Subdomain patterns

All auth modes (static tokens and auth provider) use the same glob-style wildcard matching:

| Pattern | Matches | Does NOT match |
|---------|---------|----------------|
| `myapp` | `myapp` | `myapp-dev` |
| `myapp-*` | `myapp-dev`, `myapp-feat-123` | `myapp` |
| `*-preview` | `feat-preview`, `hotfix-preview` | `preview` |
| `pr-*-acme` | `pr-123-acme`, `pr-abc-acme` | `pr-123` |
| `*` | anything | |

Rules: one `*` per pattern, case-sensitive, no `?` or `**`.

## 5. Run the Relay

### Using mcpr

Run in foreground — your process supervisor (systemd, Docker, terminal) owns the PID:

```bash
mcpr relay run relay.toml
```

Run in the background (terminal use, multi-process box):

```bash
mcpr relay run --background relay.toml
```

Other lifecycle commands:

```bash
mcpr relay status             # show PID, port, uptime
mcpr relay stop               # send SIGTERM, clean up lockfile
mcpr relay restart             # stop + start from saved config
```

`mcpr relay` commands do not require `mode = "relay"` in the config file — the mode is implicit.

Running a relay and proxy on the same machine:

```bash
mcpr proxy run --background gateway.toml      # MCP proxy
mcpr relay run --background relay.toml        # relay server
mcpr proxy stop --all && mcpr relay stop      # tear down
```

### Using Docker

The standard mcpr image works for both proxy and relay — override the command:

```bash
docker run -d \
  --name mcpr-relay \
  --restart unless-stopped \
  -p 8080:8080 \
  -v ./relay.toml:/app/relay.toml \
  ghcr.io/pragmalabs-tech/mcpr:latest \
  relay run /app/relay.toml
```

Update:

```bash
docker pull ghcr.io/pragmalabs-tech/mcpr:latest
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
mcpr proxy run mcpr.toml
# Should print: Tunnel: https://xxxxxx.tunnel.yourdomain.com
```

## 7. Verify

```bash
# Check relay is reachable (expects 400 "missing token" — means relay is running)
curl -s https://tunnel.yourdomain.com/_tunnel/register

# If running via mcpr CLI
mcpr relay status

# Full test: start a client proxy with tunnel enabled
mcpr proxy run --background mcpr.toml
mcpr proxy list
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
