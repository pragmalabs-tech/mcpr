# CSP

The proxy rewrites `_meta.openai/widgetCSP` and `_meta.ui.csp` on MCP responses so widgets receive a correct Content Security Policy regardless of which host renders them. You declare the policy once in `mcpr.toml`; every host reads the key it understands.

## Why

MCP Apps widgets run in a sandboxed iframe. The host synthesises the iframe CSP from domains the MCP server declares on the widget resource. Two problems appear in practice:

1. Hosts disagree about the schema. ChatGPT reads `openai/widgetCSP` (snake_case). Claude and VS Code read `ui.csp` (camelCase). A server that declares only one format breaks on the hosts that read the other.
2. Servers under-declare. Local development URLs (`localhost:4444`, upstream host itself) leak into the declaration and break the widget at the host.

The proxy solves both by taking a single declarative policy and emitting it to both shapes after merging with whatever the upstream declared.

## Minimal config

```toml
[csp.connectDomains]
domains = ["api.example.com"]

[csp.resourceDomains]
domains = ["cdn.example.com"]
```

Defaults: `mode = "extend"` for connect and resource, `mode = "replace"` for frame. Frames fail closed.

## Widget domain

`csp.domain` is the bare host (no scheme) this proxy is served from publicly. It feeds two things:

1. **`_meta.openai/widgetDomain`** — written on every widget meta. ChatGPT reads this field. `_meta.ui.domain` is *not* written: Claude validates that field against a hash it derives from the proxy URL itself (`{sha256(url)[:32]}.claudemcpcontent.com`), so any value an MCP layer supplies is rejected. Leaving the field absent lets Claude compute the value it expects.
2. **The proxy-URL injected into `connect` and `resource` CSP arrays** — widgets need a reachable origin to call back to the proxy for JSON-RPC and asset loads.

```toml
[csp]
domain = "widgets.example.com"
```

Resolution order:

1. `csp.domain` if set.
2. Else the tunnel URL (when `[tunnel].enabled = true`).
3. Else — local-only dev — **no public origin is available**, so the proxy *skips* the CSP injection and leaves any upstream domain field untouched. `localhost` is never written into widget CSP or the domain fields; shipping it to a host would be invalid, and cluttering the local output with it helps no one.

Only `openai/widgetDomain` is written. `_meta.ui.domain` carries Claude-specific semantics — Claude derives the expected value (`{sha256(url)[:32]}.claudemcpcontent.com`) from the proxy URL itself and rejects any other value — so the proxy leaves it alone. ChatGPT, which has no equivalent check, still gets the operator-declared host through `openai/widgetDomain`.

## Directives

Three independent directive arrays, each a sub-table:

| Directive | Controls | Default mode |
|---|---|---|
| `connectDomains` | `fetch`, `WebSocket`, `EventSource` | `extend` |
| `resourceDomains` | scripts, styles, images, fonts, media | `extend` |
| `frameDomains` | nested `<iframe>` content | `replace` |

Each directive takes two fields:

```toml
[csp.connectDomains]
domains = ["api.example.com"]    # list of origins to allow
mode    = "extend"                # "extend" or "replace"
```

## Modes

- **`extend`** — combine upstream-declared domains with the domains in this block. Upstream entries referencing `localhost`, `127.0.0.1`, or the upstream MCP host are stripped.
- **`replace`** — ignore upstream for this directive; allow only the domains in this block.

## Per-widget overrides

Add an `[[csp.widget]]` block when one widget needs domains the global policy does not grant.

```toml
[[csp.widget]]
match              = "ui://widget/payment*"
connectDomains     = ["api.stripe.com"]
connectDomainsMode = "extend"
resourceDomains    = ["js.stripe.com"]
resourceDomainsMode = "extend"
```

- `match` is a glob over the resource URI. `*` matches any sequence, `?` matches one character.
- Each directive carries its own paired `<directive>` + `<directive>Mode` fields. Omitting both leaves that directive untouched by this widget.
- `mode = "replace"` with an empty `domains` list explicitly clears the directive for matching widgets.

## How the merge works

For each directive, per response:

1. Start with upstream's declared domains (from both CSP shapes). Drop if the global mode is `replace`.
2. Strip localhost and the upstream MCP host.
3. Append the global directive's declared domains.
4. For each matching `[[csp.widget]]` entry in config order, extend or replace per the widget's directive mode.
5. For `connect` and `resource`, prepend the proxy URL if a public origin is available (see [Public widget domain](#public-widget-domain)). A loopback URL is never prepended. Deduplicate.

Replace semantics are scoped: a global replace only ignores upstream; a widget replace wipes everything accumulated before it.

The proxy URL is deliberately **not** prepended to `frame`. Widgets don't iframe the proxy back into themselves, so including it there is dead weight — and it makes every mcpr-proxied widget look like an iframe-embedder to hosts like ChatGPT, which flag that shape for additional security review.

## What the proxy emits

The merged CSP list lands in both shapes on every widget meta. The widget domain is written into `openai/widgetDomain` only — `_meta.ui.domain` is left to Claude:

```json
{
  "_meta": {
    "openai/widgetDomain": "proxy.example.com",
    "openai/widgetCSP": {
      "connect_domains": ["https://proxy.example.com", "https://api.example.com"],
      "resource_domains": [...],
      "frame_domains": [...]
    },
    "ui": {
      "csp": {
        "connectDomains": ["https://proxy.example.com", "https://api.example.com"],
        "resourceDomains": [...],
        "frameDomains": [...]
      }
    }
  }
}
```

Hosts ignore keys they do not understand, so emitting both is safe everywhere.

The proxy URL appears first in `connect_domains` / `connectDomains` and `resource_domains` / `resourceDomains` so widgets can call back to the proxy and load their assets from it. `frame_domains` / `frameDomains` contains only what the operator (and upstream, when not in replace mode) declared.

Non-widget meta (for example, a `tools/call` result with no widget indicators) is left untouched.

## Example — full config

```toml
[csp.connectDomains]
domains = ["api.myshop.com"]
mode    = "extend"

[csp.resourceDomains]
domains = ["cdn.myshop.com"]
mode    = "extend"

[csp.frameDomains]
domains = []
mode    = "replace"

[[csp.widget]]
match              = "ui://widget/payment*"
connectDomains     = ["api.stripe.com"]
connectDomainsMode = "extend"
resourceDomains    = ["js.stripe.com"]
resourceDomainsMode = "extend"
```

## Legacy shape

The older flat form is still accepted for one release:

```toml
[csp]
mode    = "extend"
domains = ["api.example.com"]
```

Loads into `connectDomains` and `resourceDomains` with the given mode. `mode = "override"` maps to `replace`. Both forms emit a warning under `mcpr validate`; migrate to the per-directive shape.

## Field reference

| Field | Type | Default | Description |
|---|---|---|---|
| `csp.domain` | `string` | — | Bare public host (no scheme). Feeds `openai/widgetDomain` and CSP injection. `_meta.ui.domain` is left to Claude, which derives it from the proxy URL. Falls back to tunnel URL when unset; suppresses injection in local-only mode. |
| `[csp.connectDomains].domains` | `string[]` | `[]` | Domains allowed for `connect-src` |
| `[csp.connectDomains].mode` | `"extend" \| "replace"` | `"extend"` | Merge mode with upstream |
| `[csp.resourceDomains].domains` | `string[]` | `[]` | Domains allowed for scripts, styles, images, fonts, media |
| `[csp.resourceDomains].mode` | `"extend" \| "replace"` | `"extend"` | Merge mode with upstream |
| `[csp.frameDomains].domains` | `string[]` | `[]` | Domains allowed for nested iframes |
| `[csp.frameDomains].mode` | `"extend" \| "replace"` | `"replace"` | Merge mode with upstream |
| `[[csp.widget]].match` | `string` | — (required) | URI glob selecting which resources this override applies to |
| `[[csp.widget]].connectDomains` | `string[]` | `[]` | Override domains for `connect` |
| `[[csp.widget]].connectDomainsMode` | `"extend" \| "replace"` | `"extend"` | Override mode for `connect` |
| `[[csp.widget]].resourceDomains` | `string[]` | `[]` | Override domains for `resource` |
| `[[csp.widget]].resourceDomainsMode` | `"extend" \| "replace"` | `"extend"` | Override mode for `resource` |
| `[[csp.widget]].frameDomains` | `string[]` | `[]` | Override domains for `frame` |
| `[[csp.widget]].frameDomainsMode` | `"extend" \| "replace"` | `"extend"` | Override mode for `frame` |
