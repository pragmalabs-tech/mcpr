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
5. Prepend the proxy URL and deduplicate.

Replace semantics are scoped: a global replace only ignores upstream; a widget replace wipes everything accumulated before it.

## What the proxy emits

The merged domain list lands in both shapes on every widget meta:

```json
{
  "_meta": {
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

The proxy URL always appears first so widgets can reach the proxy.

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
