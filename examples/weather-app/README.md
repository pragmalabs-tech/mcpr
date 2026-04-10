# Weather MCP App

A minimal MCP App with a weather widget — shows how mcpr handles CSP, widget serving, and structured content.

## Quick Start

```bash
# 1. Install dependencies
npm install

# 2. Start the MCP server
npm start
# → MCP server running on http://localhost:9000

# 3. In another terminal, start mcpr
mcpr start --mcp http://localhost:9001/mcp
# → mcpr daemon started (PID: ..., port: ...)
```

Paste the tunnel URL into ChatGPT or Claude as an MCP server. Ask it to "get the weather in Tokyo".

## What's Inside

```
server.ts          MCP server with a get_weather tool
widget/index.html  Weather card UI rendered inside the AI client
```

## How It Works

1. **`server.ts`** registers a `get_weather` tool that returns weather data as `structuredContent`, plus a `ui://` resource pointing to the widget HTML.

2. **`widget/index.html`** renders the weather card. The AI client (ChatGPT/Claude) passes the structured content to the widget via `postMessage`.

3. **mcpr** sits in front, doing three things:
   - Routes JSON-RPC requests to your MCP server
   - Serves widget HTML from `./widget/`
   - Reads `_meta.ui.csp` from your resource and injects the correct CSP headers so Google Fonts load inside the sandbox

Without mcpr, the fonts would be silently blocked by the iframe sandbox CSP.

## CSP Declarations

The server declares CSP domains in the resource metadata:

```typescript
_meta: {
  ui: {
    csp: {
      resourceDomains: [
        "https://fonts.googleapis.com",
        "https://fonts.gstatic.com",
      ],
    },
  },
}
```

mcpr reads this and injects the correct `Content-Security-Policy` headers automatically. Add any external domains your widget needs here.

## Local Development

Use `npm run dev` for auto-reload on server changes. The widget is plain HTML — edit and refresh.

```bash
# Terminal 1: MCP server with auto-reload
npm run dev

# Terminal 2: mcpr in foreground for dev
mcpr start --foreground --mcp http://localhost:9001/mcp
```
