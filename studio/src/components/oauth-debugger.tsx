import { useEffect, useRef, useState } from "react";
import { useStore } from "@/lib/store";
import { Button } from "@/components/ui/button";
import {
  STEP_LABELS,
  type OAuthDebugEvent,
  type ComplianceCheck,
  type DecodedToken,
} from "@/lib/oauth-debug";

// ── Sub-tab types ──

type DebugTab = "timeline" | "metadata" | "token";

// ── Timeline Entry ──

function TimelineEntry({ event }: { event: OAuthDebugEvent }) {
  const [expanded, setExpanded] = useState(false);
  const [copiedReq, setCopiedReq] = useState(false);
  const [copiedResp, setCopiedResp] = useState(false);

  const statusIcon =
    event.status === "success"
      ? "text-green-500"
      : event.status === "error"
        ? "text-red-400"
        : "text-yellow-500 animate-pulse";

  const statusDot =
    event.status === "pending" ? "○" : event.status === "success" ? "●" : "●";

  const copy = (text: string, setter: (v: boolean) => void) => {
    navigator.clipboard.writeText(text);
    setter(true);
    setTimeout(() => setter(false), 1500);
  };

  return (
    <div
      className="border-b border-border/30 cursor-pointer hover:bg-secondary/30 transition-colors"
      onClick={() => setExpanded(!expanded)}
    >
      <div className="flex items-center gap-2 px-3 py-1.5">
        <span className={`text-xs ${statusIcon}`}>{statusDot}</span>
        <span className="text-[10px] text-muted-foreground font-mono w-20 shrink-0">
          {event.time}
        </span>
        <span className="text-xs font-semibold text-purple-400 shrink-0">
          {STEP_LABELS[event.step]}
        </span>
        {event.request && (
          <span className="text-[10px] text-muted-foreground font-mono truncate">
            {event.request.method} {event.request.url}
          </span>
        )}
        <span className="ml-auto flex items-center gap-2 shrink-0">
          {event.response && (
            <span
              className={`text-[10px] font-mono ${
                event.response.status >= 400
                  ? "text-red-400"
                  : event.response.status >= 300
                    ? "text-yellow-400"
                    : "text-green-500"
              }`}
            >
              {event.response.status}
            </span>
          )}
          {event.durationMs !== undefined && (
            <span className="text-[10px] text-muted-foreground/60">
              {event.durationMs}ms
            </span>
          )}
          <span className="text-[8px] text-muted-foreground/50">
            {expanded ? "▼" : "▶"}
          </span>
        </span>
      </div>

      {/* Hint */}
      {event.hint && !expanded && (
        <div className="px-3 pb-1.5 pl-9">
          <p className="text-[10px] text-yellow-400/80">{event.hint}</p>
        </div>
      )}

      {/* Error */}
      {event.error && !expanded && (
        <div className="px-3 pb-1.5 pl-9">
          <p className="text-[10px] text-red-400">{event.error}</p>
        </div>
      )}

      {/* Expanded details */}
      {expanded && (
        <div className="px-3 pb-2 pl-9 space-y-2">
          {event.hint && (
            <div className="bg-yellow-500/10 border border-yellow-500/20 rounded px-2 py-1.5">
              <p className="text-[10px] text-yellow-400">{event.hint}</p>
            </div>
          )}

          {event.error && (
            <div className="bg-red-500/10 border border-red-500/20 rounded px-2 py-1.5">
              <p className="text-[10px] text-red-400">{event.error}</p>
            </div>
          )}

          {/* Request */}
          {event.request && (
            <div>
              <div className="flex items-center justify-between mb-0.5">
                <span className="text-[10px] font-semibold text-muted-foreground uppercase">
                  Request
                </span>
                <button
                  onClick={(e) => {
                    e.stopPropagation();
                    copy(JSON.stringify(event.request, null, 2), setCopiedReq);
                  }}
                  className="text-[10px] text-muted-foreground hover:text-foreground px-1"
                >
                  {copiedReq ? "Copied" : "Copy"}
                </button>
              </div>
              <pre className="bg-secondary/50 rounded px-2 py-1.5 text-[10px] font-mono text-muted-foreground overflow-x-auto max-h-40 overflow-y-auto">
                {event.request.method} {event.request.url}
                {event.request.headers &&
                  Object.entries(event.request.headers).map(
                    ([k, v]) => `\n${k}: ${v}`
                  )}
                {event.request.body && `\n\n${event.request.body}`}
              </pre>
            </div>
          )}

          {/* Response */}
          {event.response && (
            <div>
              <div className="flex items-center justify-between mb-0.5">
                <span className="text-[10px] font-semibold text-muted-foreground uppercase">
                  Response
                </span>
                <button
                  onClick={(e) => {
                    e.stopPropagation();
                    copy(
                      JSON.stringify(event.response, null, 2),
                      setCopiedResp
                    );
                  }}
                  className="text-[10px] text-muted-foreground hover:text-foreground px-1"
                >
                  {copiedResp ? "Copied" : "Copy"}
                </button>
              </div>
              <pre className="bg-secondary/50 rounded px-2 py-1.5 text-[10px] font-mono text-muted-foreground overflow-x-auto max-h-40 overflow-y-auto">
                {event.response.status} {event.response.statusText}
                {event.response.headers &&
                  Object.entries(event.response.headers).map(
                    ([k, v]) => `\n${k}: ${v}`
                  )}
                {event.response.body &&
                  `\n\n${formatBody(event.response.body)}`}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function formatBody(body: string): string {
  try {
    return JSON.stringify(JSON.parse(body), null, 2);
  } catch {
    return body;
  }
}

// ── Compliance Checker ──

function CompliancePanel({ checks }: { checks: ComplianceCheck[] }) {
  if (checks.length === 0) {
    return (
      <p className="text-center text-muted-foreground text-xs py-6">
        Run metadata discovery to see compliance checks.
      </p>
    );
  }

  return (
    <div className="py-1">
      {checks.map((check, i) => (
        <div
          key={i}
          className="flex items-start gap-2 px-3 py-1.5 border-b border-border/30"
        >
          <span
            className={`text-xs mt-0.5 shrink-0 ${
              check.status === "pass"
                ? "text-green-500"
                : check.status === "warn"
                  ? "text-yellow-400"
                  : "text-red-400"
            }`}
          >
            {check.status === "pass"
              ? "●"
              : check.status === "warn"
                ? "●"
                : "●"}
          </span>
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <span className="text-xs font-mono text-foreground">
                {check.field}
              </span>
            </div>
            <p className="text-[10px] text-muted-foreground truncate">
              {check.value}
            </p>
            <p
              className={`text-[10px] ${
                check.status === "pass"
                  ? "text-green-500/70"
                  : check.status === "warn"
                    ? "text-yellow-400/70"
                    : "text-red-400/70"
              }`}
            >
              {check.message}
            </p>
          </div>
        </div>
      ))}
    </div>
  );
}

// ── Token Inspector ──

function TokenPanel({ decoded }: { decoded: DecodedToken | null }) {
  const [copiedRaw, setCopiedRaw] = useState(false);

  if (!decoded) {
    return (
      <p className="text-center text-muted-foreground text-xs py-6">
        No token available. Complete OAuth flow to inspect token.
      </p>
    );
  }

  const copy = (text: string) => {
    navigator.clipboard.writeText(text);
    setCopiedRaw(true);
    setTimeout(() => setCopiedRaw(false), 1500);
  };

  return (
    <div className="py-1 space-y-2 px-3">
      <div className="flex items-center justify-between">
        <span className="text-[10px] font-semibold text-muted-foreground uppercase">
          Type: {decoded.isJwt ? "JWT" : "Opaque"}
        </span>
        <button
          onClick={() => copy(decoded.raw)}
          className="text-[10px] text-muted-foreground hover:text-foreground px-1"
        >
          {copiedRaw ? "Copied" : "Copy Raw"}
        </button>
      </div>

      {decoded.isJwt && decoded.header && (
        <div>
          <span className="text-[10px] font-semibold text-muted-foreground uppercase block mb-0.5">
            Header
          </span>
          <pre className="bg-secondary/50 rounded px-2 py-1.5 text-[10px] font-mono text-muted-foreground overflow-x-auto">
            {JSON.stringify(decoded.header, null, 2)}
          </pre>
        </div>
      )}

      {decoded.isJwt && decoded.payload && (
        <div>
          <span className="text-[10px] font-semibold text-muted-foreground uppercase block mb-0.5">
            Payload
          </span>
          <pre className="bg-secondary/50 rounded px-2 py-1.5 text-[10px] font-mono text-muted-foreground overflow-x-auto max-h-48 overflow-y-auto">
            {JSON.stringify(decoded.payload, null, 2)}
          </pre>
        </div>
      )}

      {decoded.expiresAt && (
        <div className="flex items-center gap-2">
          <span className="text-[10px] font-semibold text-muted-foreground uppercase">
            Expires:
          </span>
          <span
            className={`text-[10px] font-mono ${
              decoded.expiresAt.getTime() < Date.now()
                ? "text-red-400"
                : "text-green-500"
            }`}
          >
            {decoded.expiresAt.toISOString()}
          </span>
        </div>
      )}

      {decoded.scopes && (
        <div>
          <span className="text-[10px] font-semibold text-muted-foreground uppercase block mb-0.5">
            Scopes
          </span>
          <div className="flex flex-wrap gap-1">
            {decoded.scopes.map((scope) => (
              <span
                key={scope}
                className="px-1.5 py-0.5 rounded bg-primary/20 text-primary text-[10px]"
              >
                {scope}
              </span>
            ))}
          </div>
        </div>
      )}

      {!decoded.isJwt && (
        <div>
          <span className="text-[10px] font-semibold text-muted-foreground uppercase block mb-0.5">
            Raw Token
          </span>
          <pre className="bg-secondary/50 rounded px-2 py-1.5 text-[10px] font-mono text-muted-foreground break-all">
            {decoded.raw.length > 200
              ? decoded.raw.slice(0, 200) + "..."
              : decoded.raw}
          </pre>
        </div>
      )}
    </div>
  );
}

// ── PKCE Debug ──

function PkcePanel() {
  const baseUrl = useStore(() => {
    // Get base URL from the api module
    const params = new URLSearchParams(window.location.search);
    const proxy = params.get("proxy");
    if (proxy) return proxy.replace(/\/+$/, "");
    if (import.meta.env.DEV) return "http://localhost:3000";
    return window.location.origin;
  });

  const verifier = localStorage.getItem(
    `mcpr_oauth_${new URL(baseUrl).origin}_pkce_verifier`
  );
  const state = localStorage.getItem(
    `mcpr_oauth_${new URL(baseUrl).origin}_pkce_state`
  );

  if (!verifier && !state) {
    return (
      <p className="text-[10px] text-muted-foreground px-3 py-2">
        PKCE parameters are generated when you start the OAuth flow and cleared
        after token exchange.
      </p>
    );
  }

  return (
    <div className="px-3 py-2 space-y-1.5">
      {verifier && (
        <div>
          <span className="text-[10px] font-semibold text-muted-foreground uppercase block">
            code_verifier
          </span>
          <pre className="text-[10px] font-mono text-muted-foreground break-all">
            {verifier}
          </pre>
        </div>
      )}
      {state && (
        <div>
          <span className="text-[10px] font-semibold text-muted-foreground uppercase block">
            state
          </span>
          <pre className="text-[10px] font-mono text-muted-foreground break-all">
            {state}
          </pre>
        </div>
      )}
      <p className="text-[10px] text-muted-foreground/60">
        Method: S256 (SHA-256 + base64url)
      </p>
    </div>
  );
}

// ── Flow Steps Visualization ──

interface FlowStepDef {
  key: string;
  label: string;
  description: string;
}

const FLOW_STEPS: FlowStepDef[] = [
  {
    key: "metadata_discovery",
    label: "Discovery",
    description: "GET /.well-known/oauth-authorization-server",
  },
  {
    key: "client_registration",
    label: "Registration",
    description: "POST /register (RFC 7591)",
  },
  {
    key: "authorization",
    label: "Authorize",
    description: "User authorizes via browser",
  },
  {
    key: "token_exchange",
    label: "Token",
    description: "POST /token with code + PKCE verifier",
  },
  {
    key: "token_refresh",
    label: "Refresh",
    description: "POST /token with refresh_token",
  },
];

type StepStatus = "idle" | "pending" | "success" | "error" | "skipped";

function deriveStepStatuses(
  events: OAuthDebugEvent[],
  oauthStatus: string
): Record<string, StepStatus> {
  const statuses: Record<string, StepStatus> = {};

  for (const step of FLOW_STEPS) {
    // Find the latest event for this step (excluding endpoint_test)
    const stepEvents = events.filter(
      (e) => e.step === step.key && e.step !== "endpoint_test"
    );
    const latest = stepEvents[stepEvents.length - 1];

    if (latest) {
      if (latest.status === "pending") statuses[step.key] = "pending";
      else if (latest.status === "success") statuses[step.key] = "success";
      else statuses[step.key] = "error";
    } else {
      statuses[step.key] = "idle";
    }
  }

  // Mark the currently active step as pending based on oauth status
  if (
    oauthStatus === "discovering" &&
    statuses["metadata_discovery"] === "idle"
  )
    statuses["metadata_discovery"] = "pending";
  if (
    oauthStatus === "registering" &&
    statuses["client_registration"] === "idle"
  )
    statuses["client_registration"] = "pending";
  if (oauthStatus === "authorizing" && statuses["authorization"] === "idle")
    statuses["authorization"] = "pending";
  if (oauthStatus === "exchanging" && statuses["token_exchange"] === "idle")
    statuses["token_exchange"] = "pending";

  return statuses;
}

function FlowVisualization({
  events,
  oauthStatus,
}: {
  events: OAuthDebugEvent[];
  oauthStatus: string;
}) {
  const statuses = deriveStepStatuses(events, oauthStatus);

  // Find index of the first error or last completed step
  let failedIndex = -1;
  for (let i = 0; i < FLOW_STEPS.length; i++) {
    if (statuses[FLOW_STEPS[i].key] === "error") {
      failedIndex = i;
      break;
    }
  }

  return (
    <div className="px-3 py-3 border-b border-border/50 bg-secondary/20">
      <div className="flex items-start gap-0">
        {FLOW_STEPS.map((step, i) => {
          const status = statuses[step.key];
          return (
            <div key={step.key} className="flex items-start flex-1 min-w-0">
              {/* Step node */}
              <div className="flex flex-col items-center flex-1 min-w-0">
                {/* Circle */}
                <div
                  className={`w-6 h-6 rounded-full flex items-center justify-center text-[10px] font-bold shrink-0 ${
                    status === "success"
                      ? "bg-green-500/20 text-green-500 ring-1 ring-green-500/40"
                      : status === "error"
                        ? "bg-red-500/20 text-red-400 ring-1 ring-red-500/40"
                        : status === "pending"
                          ? "bg-yellow-500/20 text-yellow-500 ring-1 ring-yellow-500/40 animate-pulse"
                          : "bg-secondary text-foreground ring-1 ring-border"
                  }`}
                >
                  {status === "success"
                    ? "\u2713"
                    : status === "error"
                      ? "!"
                      : status === "pending"
                        ? "\u2022"
                        : i + 1}
                </div>

                {/* Label */}
                <span
                  className={`text-[9px] font-semibold mt-1 text-center leading-tight ${
                    status === "success"
                      ? "text-green-500"
                      : status === "error"
                        ? "text-red-400"
                        : status === "pending"
                          ? "text-yellow-500"
                          : "text-muted-foreground/40"
                  }`}
                >
                  {step.label}
                </span>

                {/* Description */}
                <span className="text-[8px] text-muted-foreground/50 text-center mt-0.5 leading-tight px-1 hidden sm:block">
                  {step.description}
                </span>
              </div>

              {/* Connector line */}
              {i < FLOW_STEPS.length - 1 && (
                <div className="flex items-center pt-3 -mx-1">
                  <div
                    className={`h-px w-4 ${
                      status === "success"
                        ? "bg-green-500/40"
                        : status === "error"
                          ? "bg-red-500/40"
                          : "bg-border"
                    }`}
                  />
                  <span
                    className={`text-[8px] ${
                      status === "success"
                        ? "text-green-500/40"
                        : status === "error"
                          ? "text-red-500/40"
                          : "text-muted-foreground/20"
                    }`}
                  >
                    {"\u203a"}
                  </span>
                </div>
              )}
            </div>
          );
        })}
      </div>

      {/* Error callout */}
      {failedIndex >= 0 && (
        <div className="mt-2 bg-red-500/10 border border-red-500/20 rounded px-2 py-1.5">
          <p className="text-[10px] text-red-400">
            Flow stopped at step {failedIndex + 1}:{" "}
            <strong>{FLOW_STEPS[failedIndex].label}</strong>
            {" — "}
            {events
              .filter(
                (e) =>
                  e.step === FLOW_STEPS[failedIndex].key && e.status === "error"
              )
              .slice(-1)
              .map(
                (e) => e.hint || e.error || `HTTP ${e.response?.status}`
              )[0] || "check timeline for details"}
          </p>
        </div>
      )}
    </div>
  );
}

// ── Main Debugger Component ──

export function OAuthDebugger() {
  const { oauthDebugEvents, clearOAuthDebugEvents, oauth, testOAuthEndpoints } =
    useStore();

  const [tab, setTab] = useState<DebugTab>("timeline");
  const [testing, setTesting] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [oauthDebugEvents]);

  const handleTestEndpoints = async () => {
    setTesting(true);
    try {
      await testOAuthEndpoints();
    } finally {
      setTesting(false);
    }
  };

  const passCount = oauth.complianceChecks.filter(
    (c) => c.status === "pass"
  ).length;
  const warnCount = oauth.complianceChecks.filter(
    (c) => c.status === "warn"
  ).length;
  const failCount = oauth.complianceChecks.filter(
    (c) => c.status === "fail"
  ).length;

  return (
    <div className="flex-1 flex flex-col min-h-0">
      {/* Header with sub-tabs */}
      <div className="flex items-center justify-between px-3 py-1.5 bg-secondary/50 shrink-0">
        <span className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
          OAuth 2.1 Debugger
        </span>
        <div className="flex items-center gap-1">
          {tab === "timeline" && (
            <>
              <Button
                variant="ghost"
                size="sm"
                className="h-5 text-[10px] px-1.5"
                onClick={handleTestEndpoints}
                disabled={testing}
              >
                {testing ? "Testing..." : "Test Endpoints"}
              </Button>
              <Button
                variant="ghost"
                size="sm"
                className="h-5 text-[10px] px-1.5"
                onClick={clearOAuthDebugEvents}
              >
                Clear
              </Button>
            </>
          )}
        </div>
      </div>

      {/* Sub-tabs */}
      <div className="flex border-b shrink-0">
        <button
          onClick={() => setTab("timeline")}
          className={`px-3 py-1 text-[10px] font-semibold uppercase tracking-wider transition-colors ${
            tab === "timeline"
              ? "text-foreground border-b-2 border-primary"
              : "text-muted-foreground hover:text-foreground"
          }`}
        >
          Timeline
          {oauthDebugEvents.length > 0 && (
            <span className="ml-1 text-muted-foreground/60">
              ({oauthDebugEvents.length})
            </span>
          )}
        </button>
        <button
          onClick={() => setTab("metadata")}
          className={`px-3 py-1 text-[10px] font-semibold uppercase tracking-wider transition-colors flex items-center gap-1 ${
            tab === "metadata"
              ? "text-foreground border-b-2 border-primary"
              : "text-muted-foreground hover:text-foreground"
          }`}
        >
          Compliance
          {oauth.complianceChecks.length > 0 && (
            <span className="flex items-center gap-0.5">
              {passCount > 0 && (
                <span className="text-green-500">{passCount}</span>
              )}
              {warnCount > 0 && (
                <span className="text-yellow-400">{warnCount}</span>
              )}
              {failCount > 0 && (
                <span className="text-red-400">{failCount}</span>
              )}
            </span>
          )}
        </button>
        <button
          onClick={() => setTab("token")}
          className={`px-3 py-1 text-[10px] font-semibold uppercase tracking-wider transition-colors ${
            tab === "token"
              ? "text-foreground border-b-2 border-primary"
              : "text-muted-foreground hover:text-foreground"
          }`}
        >
          Token
        </button>
      </div>

      {/* Content */}
      <div ref={scrollRef} className="flex-1 overflow-y-auto min-h-0">
        {tab === "timeline" && (
          <>
            {/* Flow visualization — always visible */}
            <FlowVisualization
              events={oauthDebugEvents}
              oauthStatus={oauth.status}
            />

            {oauthDebugEvents.length === 0 ? (
              <div className="text-center py-6 space-y-2">
                <p className="text-muted-foreground text-xs">
                  No OAuth events yet.
                </p>
                <p className="text-muted-foreground/60 text-[10px]">
                  Start an OAuth flow or test endpoints to see requests here.
                </p>
              </div>
            ) : (
              <div>
                {oauthDebugEvents.map((event) => (
                  <TimelineEntry key={event.id} event={event} />
                ))}
              </div>
            )}
          </>
        )}

        {tab === "metadata" && (
          <div>
            <CompliancePanel checks={oauth.complianceChecks} />
            {oauth.complianceChecks.length > 0 && (
              <div className="border-t">
                <div className="px-3 py-1.5">
                  <span className="text-[10px] font-semibold text-muted-foreground uppercase">
                    PKCE Parameters
                  </span>
                </div>
                <PkcePanel />
              </div>
            )}
          </div>
        )}

        {tab === "token" && <TokenPanel decoded={oauth.decodedToken} />}
      </div>
    </div>
  );
}
