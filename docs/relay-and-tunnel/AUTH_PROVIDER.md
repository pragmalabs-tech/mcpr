# Building an Auth Provider for mcpr Relay

The mcpr relay server supports three auth modes:

1. **Open** -- no auth, anyone can tunnel (default)
2. **Static tokens** -- hardcoded in `mcpr.toml`, no external service needed (see [DEPLOY_RELAY_SERVER.md](DEPLOY_RELAY_SERVER.md))
3. **Auth provider** -- external API for dynamic token management (this document)

If you just need a few developers with fixed tokens, use **static tokens**. Use an auth provider when you need dynamic token management, user registration, or revocation at scale.

This document describes the API contract your auth provider must implement, the subdomain pattern format, and example implementations.

## Overview

```
Developer's mcpr client
    |
    | WS /_tunnel/register?token=TOKEN&subdomain=myapp
    v
mcpr relay
    |
    | POST /api/verify  (if auth_provider configured)
    v
Your Auth Provider
    |
    | 200: allowed subdomains
    | 401: invalid token
    | 403: subdomain not allowed
    v
mcpr relay
    |
    | allows or rejects the tunnel
    v
Developer's mcpr client
```

The relay has **no database and no user management**. It simply asks your auth provider: "Is this token valid, and can it use this subdomain?" Your provider makes the decision.

## API Contract

Your auth provider must implement a single endpoint:

### `POST /api/verify`

**Request:**

```http
POST /api/verify HTTP/1.1
Content-Type: application/json
X-Relay-Secret: <shared_secret>

{
  "token": "mcpr_a1b2c3d4e5f6...",
  "subdomain": "myapp-dev"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `token` | string | The developer's API token (from `[tunnel].token` in their `mcpr.toml`) |
| `subdomain` | string | The subdomain the developer is requesting |

**Headers:**

| Header | Description |
|--------|-------------|
| `X-Relay-Secret` | Shared secret configured on both relay and auth provider. Use this to verify the request is actually from your relay, not a random caller. |

### Response: Allowed (200)

```json
{
  "subdomains": ["myapp", "myapp-*"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `subdomains` | string[] | List of subdomain patterns this token is authorized to use |

The relay checks if the requested subdomain matches any pattern in the list.

### Response: Invalid Token (401)

```json
{
  "error": "invalid_token"
}
```

Returned when the token doesn't exist, is expired, or has been revoked.

### Response: Subdomain Not Allowed (403)

```json
{
  "error": "subdomain_not_allowed"
}
```

Returned when the token is valid but not authorized for the requested subdomain. (You can also handle this server-side and only return 200/401 -- the relay will check patterns either way.)

## Subdomain Patterns

The `subdomains` array in the 200 response supports glob-style wildcard matching with `*`:

| Pattern | Matches | Does NOT match |
|---------|---------|----------------|
| `myapp` | `myapp` | `myapp-dev`, `other` |
| `myapp-*` | `myapp-dev`, `myapp-feat-123`, `myapp-` | `myapp`, `other` |
| `*-preview` | `feat-preview`, `hotfix-preview` | `preview`, `preview-other` |
| `pr-*-mycompany` | `pr-123-mycompany`, `pr-abc-mycompany` | `pr-123`, `pr-mycompany` |
| `*` | anything | (matches everything) |

**Rules:**
- `*` matches any sequence of characters (including empty string)
- Only one `*` per pattern
- No `?` or `**` -- just simple glob
- Patterns are case-sensitive
- Subdomains are the first dot-segment of the Host header (e.g. `myapp` from `myapp.tunnel.mcpr.app`)

### Common use cases

**Single project:**
```json
{ "subdomains": ["myapp"] }
```

**Project with branch previews (CI/CD):**
```json
{ "subdomains": ["myapp", "myapp-*"] }
```
Allows `myapp` (production) and `myapp-pr-123`, `myapp-staging`, etc.

**Company-wide wildcard:**
```json
{ "subdomains": ["*-acme"] }
```
Allows any subdomain ending in `-acme`.

**Unrestricted (use with caution):**
```json
{ "subdomains": ["*"] }
```

## Relay Configuration

```toml
mode = "relay"
port = 8081

[relay]
domain = "tunnel.yourdomain.com"
auth_provider = "https://auth.yourdomain.com"
auth_provider_secret = "your-shared-secret-here"
```

Or via environment variables:
```bash
MCPR_AUTH_PROVIDER=https://auth.yourdomain.com
MCPR_AUTH_PROVIDER_SECRET=your-shared-secret-here
```

When `auth_provider` is **not set**, the relay runs in open mode (anyone can tunnel).

## Security Considerations

### Shared Secret

The `X-Relay-Secret` header prevents unauthorized callers from probing your auth provider. Generate a strong random secret:

```bash
openssl rand -hex 32
```

Your auth provider should reject any request without a valid `X-Relay-Secret`.

### Token Storage

- **Never store tokens in plaintext.** Hash them with SHA-256 (or bcrypt/argon2 for extra security) and compare hashes.
- Token format suggestion: `mcpr_` prefix + 32 random bytes hex-encoded = `mcpr_a1b2c3...` (72 chars total). The prefix makes tokens easy to identify in logs and credential scanners.

### HTTPS

Always run your auth provider behind HTTPS. The relay sends tokens in the request body over this connection.

### Rate Limiting

Consider rate-limiting the `/api/verify` endpoint to prevent brute-force token guessing. The relay makes one call per tunnel registration (not per request), so legitimate traffic is low.

## Example Implementations

### Minimal (Node.js + Express)

A simple in-memory auth provider for testing:

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
  // Verify relay secret
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

### Cloudflare Worker + D1

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

    // Hash token for lookup
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

## Testing Your Auth Provider

Use curl to test your provider before connecting the relay:

```bash
# Should return 200 with subdomains
curl -X POST https://auth.yourdomain.com/api/verify \
  -H "Content-Type: application/json" \
  -H "X-Relay-Secret: your-secret" \
  -d '{"token": "mcpr_test_token", "subdomain": "myapp"}'

# Should return 401
curl -X POST https://auth.yourdomain.com/api/verify \
  -H "Content-Type: application/json" \
  -H "X-Relay-Secret: your-secret" \
  -d '{"token": "invalid_token", "subdomain": "myapp"}'

# Should return 401 (wrong relay secret)
curl -X POST https://auth.yourdomain.com/api/verify \
  -H "Content-Type: application/json" \
  -H "X-Relay-Secret: wrong-secret" \
  -d '{"token": "mcpr_test_token", "subdomain": "myapp"}'
```

Then test with the actual relay:

```bash
# Start relay with auth (config: relay.toml — see DEPLOY_RELAY_SERVER.md)
mcpr relay run relay.toml

# Connect client (mcpr.toml has tunnel.relay_url and tunnel.token set)
mcpr proxy run mcpr.toml

# Connect with bad token (should fail)
# Set tunnel.token = "bad_token" in mcpr.toml
```
