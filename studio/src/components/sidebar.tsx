import { useEffect, useMemo, useState } from "react";
import { isRemoteProxy, getBaseUrl } from "@/lib/api";
import { useStore } from "@/lib/store";
import type { SelectedItem } from "@/lib/store";
import { Input } from "@/components/ui/input";
import { AuthPanel } from "./auth-panel";

function displayName(name: string) {
  return name.replace(/_/g, " ");
}

export function Sidebar() {
  const { widgets, tools, resources, loading, selected, loadAll, select } =
    useStore();

  const [filter, setFilter] = useState("");
  const [sections, setSections] = useState({
    tools: true,
    widgets: true,
    resources: true,
  });
  const toggleSection = (key: keyof typeof sections) =>
    setSections((s) => ({ ...s, [key]: !s[key] }));

  useEffect(() => {
    loadAll();
  }, []);

  const q = filter.toLowerCase();
  const filteredTools = useMemo(
    () =>
      q
        ? tools.filter(
            (t) =>
              t.name.toLowerCase().includes(q) ||
              t.description?.toLowerCase().includes(q)
          )
        : tools,
    [tools, q]
  );
  const filteredWidgets = useMemo(
    () =>
      q ? widgets.filter((w) => w.name.toLowerCase().includes(q)) : widgets,
    [widgets, q]
  );
  const filteredResources = useMemo(
    () =>
      q
        ? resources.filter(
            (r) =>
              r.name?.toLowerCase().includes(q) ||
              r.uri.toLowerCase().includes(q)
          )
        : resources,
    [resources, q]
  );

  function isItemSelected(item: SelectedItem): boolean {
    if (!selected) return false;
    if (selected.type !== item.type) return false;
    if (item.type === "widget" && selected.type === "widget")
      return item.name === selected.name;
    if (item.type === "tool" && selected.type === "tool")
      return item.tool.name === selected.tool.name;
    if (item.type === "resource" && selected.type === "resource")
      return item.resource.uri === selected.resource.uri;
    return false;
  }

  const itemBtn = (item: SelectedItem, label: string, sublabel?: string) => (
    <button
      onClick={() => select(item)}
      title={sublabel || label}
      className={`w-full text-left px-3 py-1 hover:bg-secondary/50 transition-colors ${
        isItemSelected(item)
          ? "bg-secondary text-foreground"
          : "text-muted-foreground"
      }`}
    >
      <span className="block text-xs truncate">{label}</span>
      {sublabel && (
        <span className="block text-[10px] text-muted-foreground/60 truncate">
          {sublabel}
        </span>
      )}
    </button>
  );

  const sectionHeader = (
    key: keyof typeof sections,
    label: string,
    count: number
  ) => (
    <button
      onClick={() => toggleSection(key)}
      className="w-full flex items-center justify-between px-3 py-1.5 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground hover:bg-secondary/30 transition-colors"
    >
      <span>
        {label} <span className="normal-case font-normal">{count}</span>
      </span>
      <span className="text-[8px]">{sections[key] ? "▼" : "▶"}</span>
    </button>
  );

  const totalItems = tools.length + widgets.length + resources.length;

  return (
    <div className="w-72 shrink-0 border-r flex flex-col h-full">
      {/* Logo */}
      <div className="px-4 py-3 border-b shrink-0">
        <div className="flex items-center gap-2">
          <img
            src="/studio/mcpr-logo.jpg"
            alt="mcpr"
            className="w-6 h-6 rounded"
          />
          <span className="font-semibold text-sm">mcpr studio</span>
        </div>
        {isRemoteProxy() && (
          <p className="text-[10px] text-muted-foreground font-mono mt-1 truncate">
            {getBaseUrl()}
          </p>
        )}
      </div>

      {/* Auth */}
      <AuthPanel />

      {/* Search */}
      {totalItems > 5 && (
        <div className="px-3 py-2 border-b shrink-0">
          <Input
            type="text"
            placeholder="Filter…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            className="h-7 text-xs"
          />
        </div>
      )}

      {/* Sections */}
      <div className="flex-1 overflow-y-auto">
        {loading && (
          <p className="text-muted-foreground text-xs px-3 py-3">Loading…</p>
        )}

        {filteredTools.length > 0 && (
          <div>
            {sectionHeader("tools", "Tools", filteredTools.length)}
            {sections.tools &&
              filteredTools.map((t) => (
                <div key={t.name}>
                  {itemBtn(
                    { type: "tool", tool: t },
                    displayName(t.name),
                    t.description
                  )}
                </div>
              ))}
          </div>
        )}

        {filteredWidgets.length > 0 && (
          <div>
            {sectionHeader("widgets", "Widgets", filteredWidgets.length)}
            {sections.widgets &&
              filteredWidgets.map((w) => (
                <div key={w.name}>
                  {itemBtn(
                    { type: "widget", name: w.name },
                    displayName(w.name)
                  )}
                </div>
              ))}
          </div>
        )}

        {filteredResources.length > 0 && (
          <div>
            {sectionHeader("resources", "Resources", filteredResources.length)}
            {sections.resources &&
              filteredResources.map((r) => (
                <div key={r.uri}>
                  {itemBtn(
                    { type: "resource", resource: r },
                    r.name || r.uri,
                    r.description
                  )}
                </div>
              ))}
          </div>
        )}

        {!loading && totalItems === 0 && (
          <p className="text-muted-foreground text-xs px-3 py-3">
            No tools, widgets, or resources found.
          </p>
        )}
      </div>

      {/* Footer */}
      <div className="px-4 py-3 border-t shrink-0 text-[10px] text-muted-foreground">
        <div className="flex items-center gap-2">
          <a
            href="https://mcpr.app"
            target="_blank"
            rel="noopener noreferrer"
            className="hover:text-foreground transition-colors"
          >
            mcpr.app
          </a>
          <span>·</span>
          <a
            href="https://github.com/cptrodgers/mcpr"
            target="_blank"
            rel="noopener noreferrer"
            className="hover:text-foreground transition-colors"
          >
            GitHub
          </a>
        </div>
      </div>
    </div>
  );
}
