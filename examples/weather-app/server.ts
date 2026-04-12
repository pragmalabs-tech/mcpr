import { randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { isInitializeRequest } from "@modelcontextprotocol/sdk/types.js";
import {
  registerAppTool,
  registerAppResource,
  RESOURCE_MIME_TYPE,
} from "@modelcontextprotocol/ext-apps/server";
import express from "express";
import { z } from "zod";

const RESOURCE_URI = "ui://weather-app/index.html";

// Read and bundle widget at startup — inline CSS so the HTML is self-contained
// (hosts render the resource HTML string in a sandboxed iframe; relative paths won't resolve)
const __dirname = dirname(fileURLToPath(import.meta.url));
const rawHtml = readFileSync(join(__dirname, "widget", "index.html"), "utf-8");
const css = readFileSync(join(__dirname, "widget", "style.css"), "utf-8");
const WIDGET_HTML = rawHtml.replace(
  '<link rel="stylesheet" href="style.css" />',
  `<style>\n${css}\n</style>`,
);

// Mock weather data — replace with a real API in production
const WEATHER_DATA: Record<string, any> = {
  tokyo: {
    city: "Tokyo",
    temp: 22,
    condition: "Partly Cloudy",
    humidity: 65,
    wind: 12,
    icon: "⛅",
    forecast: [
      { day: "Mon", temp: 23, icon: "☀️" },
      { day: "Tue", temp: 21, icon: "🌧️" },
      { day: "Wed", temp: 24, icon: "☀️" },
      { day: "Thu", temp: 20, icon: "⛅" },
      { day: "Fri", temp: 22, icon: "☀️" },
    ],
  },
  london: {
    city: "London",
    temp: 14,
    condition: "Rainy",
    humidity: 82,
    wind: 18,
    icon: "🌧️",
    forecast: [
      { day: "Mon", temp: 13, icon: "🌧️" },
      { day: "Tue", temp: 15, icon: "⛅" },
      { day: "Wed", temp: 14, icon: "🌧️" },
      { day: "Thu", temp: 16, icon: "☀️" },
      { day: "Fri", temp: 15, icon: "⛅" },
    ],
  },
  "new york": {
    city: "New York",
    temp: 18,
    condition: "Sunny",
    humidity: 45,
    wind: 8,
    icon: "☀️",
    forecast: [
      { day: "Mon", temp: 19, icon: "☀️" },
      { day: "Tue", temp: 20, icon: "☀️" },
      { day: "Wed", temp: 17, icon: "⛅" },
      { day: "Thu", temp: 16, icon: "🌧️" },
      { day: "Fri", temp: 18, icon: "⛅" },
    ],
  },
};

function getWeather(city: string) {
  const key = city.toLowerCase().trim();
  return (
    WEATHER_DATA[key] ?? {
      city,
      temp: Math.floor(Math.random() * 30) + 5,
      condition: "Clear",
      humidity: Math.floor(Math.random() * 50) + 30,
      wind: Math.floor(Math.random() * 20) + 5,
      icon: "☀️",
      forecast: [
        { day: "Mon", temp: 20, icon: "☀️" },
        { day: "Tue", temp: 19, icon: "⛅" },
        { day: "Wed", temp: 21, icon: "☀️" },
        { day: "Thu", temp: 18, icon: "🌧️" },
        { day: "Fri", temp: 20, icon: "⛅" },
      ],
    }
  );
}

// Create a fresh MCP server with tools and resources registered
function createServer() {
  const server = new McpServer({
    name: "weather-app",
    version: "1.0.0",
  });

  // Register the widget UI resource — serves the actual HTML
  registerAppResource(server, RESOURCE_URI, RESOURCE_URI, {}, async () => ({
    contents: [
      {
        uri: RESOURCE_URI,
        mimeType: RESOURCE_MIME_TYPE,
        text: WIDGET_HTML,
        _meta: {
          ui: {
            csp: {
              // These fonts would be BLOCKED without CSP declarations.
              // mcpr reads these and injects the correct CSP headers automatically.
              resourceDomains: [
                "https://fonts.googleapis.com",
                "https://fonts.gstatic.com",
                "https://esm.sh",
              ],
            },
          },
        },
      },
    ],
  }));

  // Register the weather tool — _meta.ui.resourceUri in description links it to the widget
  registerAppTool(
    server,
    "get_weather",
    {
      title: "Get Weather",
      description: "Get current weather and 5-day forecast for a city",
      inputSchema: {
        city: z
          .string()
          .describe("City name (e.g. 'Tokyo', 'London', 'New York')"),
      },
      _meta: { ui: { resourceUri: RESOURCE_URI } },
    },
    async ({ city }) => {
      const weather = getWeather(city);
      return {
        content: [{ type: "text" as const, text: JSON.stringify(weather) }],
        structuredContent: weather,
      };
    },
  );

  return server;
}

// Express app
const app = express();
app.use(express.json());

const PORT = Number(process.env.PORT ?? 9001);

// Store transports by session ID
const transports: Record<string, StreamableHTTPServerTransport> = {};

// POST — initialize new sessions or route to existing ones
app.post("/mcp", async (req, res) => {
  const sessionId = req.headers["mcp-session-id"] as string | undefined;

  try {
    if (sessionId && transports[sessionId]) {
      // Existing session — reuse transport
      await transports[sessionId].handleRequest(req, res, req.body);
    } else if (!sessionId && isInitializeRequest(req.body)) {
      // New session
      const transport = new StreamableHTTPServerTransport({
        sessionIdGenerator: () => randomUUID(),
        onsessioninitialized: (id) => {
          transports[id] = transport;
        },
      });
      transport.onclose = () => {
        const sid = transport.sessionId;
        if (sid) delete transports[sid];
      };
      const server = createServer();
      await server.connect(transport);
      await transport.handleRequest(req, res, req.body);
    } else {
      res.status(400).json({
        jsonrpc: "2.0",
        error: { code: -32000, message: "Bad Request: No valid session ID" },
        id: null,
      });
    }
  } catch (error) {
    console.error("Error handling MCP request:", error);
    if (!res.headersSent) {
      res.status(500).json({
        jsonrpc: "2.0",
        error: { code: -32603, message: "Internal server error" },
        id: null,
      });
    }
  }
});

// GET — SSE stream for existing sessions
app.get("/mcp", async (req, res) => {
  const sessionId = req.headers["mcp-session-id"] as string | undefined;
  if (!sessionId || !transports[sessionId]) {
    res.status(400).send("Invalid or missing session ID");
    return;
  }
  await transports[sessionId].handleRequest(req, res);
});

// DELETE — session termination
app.delete("/mcp", async (req, res) => {
  const sessionId = req.headers["mcp-session-id"] as string | undefined;
  if (!sessionId || !transports[sessionId]) {
    res.status(400).send("Invalid or missing session ID");
    return;
  }
  await transports[sessionId].handleRequest(req, res);
});

app.get("/health", (_req, res) => {
  res.json({ status: "ok" });
});

app.listen(PORT, () => {
  console.log(`Weather MCP server running on http://localhost:${PORT}`);
  console.log(`MCP endpoint: http://localhost:${PORT}/mcp`);
  console.log("");
  console.log("Next: start mcpr to proxy it:");
  console.log(`  mcpr start`);
});

// Graceful shutdown — close all active transports
process.on("SIGINT", async () => {
  for (const sid of Object.keys(transports)) {
    await transports[sid].close();
    delete transports[sid];
  }
  process.exit(0);
});
