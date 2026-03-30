import { useStore } from "@/lib/store";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type { Platform, ViewportPreset } from "@/lib/store";
import { VIEWPORT_PRESETS } from "@/lib/store";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";

export function WidgetConfig() {
  const {
    platform,
    studioTheme,
    theme,
    displayMode,
    locale,
    strictMode,
    cspViolations,
    viewportPreset,
    viewportCustom,
    setStudioTheme,
    setPlatform,
    setTheme,
    setDisplayMode,
    setLocale,
    setStrictMode,
    setViewportPreset,
    setViewportCustom,
  } = useStore();

  const errorCount = cspViolations.filter((v) => v.severity === "error").length;

  return (
    <div className="border-b shrink-0 text-xs">
      {/* Row 1: Platform + Widget settings + Viewport */}
      <div className="flex items-center gap-2 px-3 py-1.5">
        <Tabs
          value={platform}
          onValueChange={(v) => setPlatform(v as Platform)}
        >
          <TabsList className="h-7">
            <TabsTrigger value="openai" className="text-xs px-2.5 h-5">
              OpenAI
            </TabsTrigger>
            <TabsTrigger value="claude" className="text-xs px-2.5 h-5">
              Claude
            </TabsTrigger>
          </TabsList>
        </Tabs>

        <Separator orientation="vertical" className="h-4" />

        <Label className="text-muted-foreground text-xs">Theme</Label>
        <Select value={theme} onValueChange={(v) => v && setTheme(v)}>
          <SelectTrigger size="sm" className="text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="light">Light</SelectItem>
            <SelectItem value="dark">Dark</SelectItem>
          </SelectContent>
        </Select>

        <Label className="text-muted-foreground text-xs">Display</Label>
        <Select
          value={displayMode}
          onValueChange={(v) => v && setDisplayMode(v)}
        >
          <SelectTrigger size="sm" className="text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="compact">Compact</SelectItem>
            <SelectItem value="inline">Inline</SelectItem>
            <SelectItem value="fullscreen">Fullscreen</SelectItem>
          </SelectContent>
        </Select>

        <Label className="text-muted-foreground text-xs">Locale</Label>
        <Input
          value={locale}
          onChange={(e) => setLocale(e.target.value)}
          className="h-7 text-xs w-20"
        />

        <Separator orientation="vertical" className="h-4" />

        <Label className="text-muted-foreground text-xs">Viewport</Label>
        <Select
          value={viewportPreset}
          onValueChange={(v) => v && setViewportPreset(v as ViewportPreset)}
        >
          <SelectTrigger size="sm" className="text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {Object.entries(VIEWPORT_PRESETS).map(([key, size]) => (
              <SelectItem key={key} value={key}>
                {key.charAt(0).toUpperCase() + key.slice(1)} ({size.width}x
                {size.height})
              </SelectItem>
            ))}
            <SelectItem value="custom">Custom</SelectItem>
          </SelectContent>
        </Select>
        {viewportPreset === "custom" && (
          <>
            <Input
              type="number"
              min={100}
              max={2560}
              value={viewportCustom.width}
              onChange={(e) =>
                setViewportCustom({
                  width: Math.min(
                    2560,
                    Math.max(100, Number(e.target.value) || 100)
                  ),
                })
              }
              className="h-7 text-xs w-16"
              title="Width (px)"
            />
            <span className="text-muted-foreground">×</span>
            <Input
              type="number"
              min={100}
              max={2560}
              value={viewportCustom.height}
              onChange={(e) =>
                setViewportCustom({
                  height: Math.min(
                    2560,
                    Math.max(100, Number(e.target.value) || 100)
                  ),
                })
              }
              className="h-7 text-xs w-16"
              title="Height (px)"
            />
          </>
        )}
      </div>

      {/* Row 2: Sandbox + Dark toggle */}
      <div className="flex items-center gap-2 px-3 py-1.5 border-t border-border/50">
        <div className="flex items-center gap-1.5">
          <Switch
            size="sm"
            checked={strictMode}
            onCheckedChange={setStrictMode}
          />
          <Label
            className="text-xs text-muted-foreground cursor-pointer"
            onClick={() => setStrictMode(!strictMode)}
          >
            Sandbox Enforcement
          </Label>
          {errorCount > 0 && (
            <Badge variant="destructive" className="text-[10px] px-1.5 py-0">
              {errorCount}
            </Badge>
          )}
        </div>

        <div className="flex-1" />

        <div className="flex items-center gap-1.5">
          <Switch
            size="sm"
            checked={studioTheme === "dark"}
            onCheckedChange={(checked) =>
              setStudioTheme(checked ? "dark" : "light")
            }
          />
          <Label
            className="text-xs text-muted-foreground cursor-pointer"
            onClick={() =>
              setStudioTheme(studioTheme === "dark" ? "light" : "dark")
            }
          >
            Dark
          </Label>
        </div>
      </div>
    </div>
  );
}
