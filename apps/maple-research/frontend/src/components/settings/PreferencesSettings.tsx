import { useEffect, useState } from "react";
import { useBlocker } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { RotateCcw } from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { useChatTypography } from "@/contexts/ChatTypographyContext";
import { useSettingsNavigationLock } from "@/contexts/SettingsNavigationLockContext";
import {
  CHAT_FONT_OPTIONS,
  CHAT_FONT_SIZE_MAX,
  CHAT_FONT_SIZE_MIN,
  CHAT_FONT_SIZE_STEP
} from "@/services/chatTypographyPreferences";
import { useTTS } from "@/services/tts/TTSContext";
import {
  isVoxtralTTSVoice,
  TTS_MAX_PLAYBACK_SPEED,
  TTS_MIN_PLAYBACK_SPEED,
  TTS_PLAYBACK_SPEED_STEP,
  VOXTRAL_TTS_VOICE_OPTIONS
} from "@/services/tts/ttsPreferences";
import { SettingsPage, SettingsSection } from "./SettingsPage";

const TTS_VOICE_GROUPS = ["Default voices", "Reference accents"] as const;

function formatPlaybackSpeed(speed: number): string {
  return `${speed.toFixed(1)}x`;
}

export function PreferencesSettings() {
  const os = useOpenSecret();
  const { fontFamily, fontSize, setFontFamily, setFontSize, resetTypography, hasCustomTypography } =
    useChatTypography();
  const {
    playbackSpeed,
    hasCustomPlaybackSpeed,
    voice,
    setPlaybackSpeed,
    resetPlaybackSpeed,
    setVoice
  } = useTTS();
  const selectedFontOption = CHAT_FONT_OPTIONS.find((option) => option.value === fontFamily);
  const [prompt, setPrompt] = useState("");
  const [instructionId, setInstructionId] = useState<string | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState(false);

  useBlocker({
    shouldBlockFn: () => isSaving,
    disabled: !isSaving,
    enableBeforeUnload: isSaving
  });
  useSettingsNavigationLock(isSaving);

  useEffect(() => {
    let active = true;

    const loadPreferences = async () => {
      setIsLoading(true);
      setError(null);
      try {
        const response = await os.listInstructions({ limit: 100 });
        if (!active) return;
        const defaultInstruction = response.data.find((instruction) => instruction.is_default);
        setInstructionId(defaultInstruction?.id ?? null);
        setPrompt(defaultInstruction?.prompt ?? "");
      } catch (loadError) {
        console.error("Failed to load preferences:", loadError);
        if (active) setError("Failed to load preferences. Please try again.");
      } finally {
        if (active) setIsLoading(false);
      }
    };

    void loadPreferences();
    return () => {
      active = false;
    };
  }, [os]);

  const handleSubmit = async (event: React.FormEvent) => {
    event.preventDefault();
    setError(null);
    setSuccess(false);
    setIsSaving(true);

    try {
      if (instructionId) {
        if (prompt.trim() === "") {
          await os.deleteInstruction(instructionId);
          setInstructionId(null);
          setPrompt("");
        } else {
          await os.updateInstruction(instructionId, { prompt });
        }
      } else if (prompt.trim() !== "") {
        const newInstruction = await os.createInstruction({
          name: "User Preferences",
          prompt,
          is_default: true
        });
        setInstructionId(newInstruction.id);
      }
      setSuccess(true);
    } catch (saveError) {
      console.error("Failed to save preferences:", saveError);
      setError("Failed to save preferences. Please try again.");
    } finally {
      setIsSaving(false);
    }
  };

  return (
    <SettingsPage
      title="Preferences"
      description="Customize the defaults Maple uses for your AI conversations."
    >
      <SettingsSection
        title="Default system prompt"
        description="This instruction is included by default when you start a conversation."
      >
        <form onSubmit={handleSubmit} className="space-y-4">
          {error && (
            <Alert variant="destructive">
              <AlertDescription>{error}</AlertDescription>
            </Alert>
          )}
          {success && (
            <Alert>
              <AlertDescription>Preferences saved successfully.</AlertDescription>
            </Alert>
          )}
          <div className="grid gap-2">
            <Label htmlFor="settings-system-prompt">System prompt</Label>
            <Textarea
              id="settings-system-prompt"
              value={prompt}
              onChange={(event) => {
                setPrompt(event.target.value);
                setSuccess(false);
                setError(null);
              }}
              placeholder="Enter your custom system prompt here..."
              className="min-h-[240px] resize-y"
              disabled={isLoading}
            />
            <p className="text-xs leading-relaxed text-muted-foreground">
              Leave this empty and save to remove your current default instruction.
            </p>
          </div>
          <div className="flex justify-end">
            <Button type="submit" disabled={isLoading || isSaving || success}>
              {isLoading ? "Loading..." : isSaving ? "Saving..." : "Save preferences"}
            </Button>
          </div>
        </form>
      </SettingsSection>
      <SettingsSection
        title="Chat appearance"
        description="Adjust the reading experience for conversations while keeping Maple's current look as the default."
      >
        <div className="space-y-6">
          <div className="grid gap-2">
            <Label htmlFor="settings-chat-font">Chat font</Label>
            <Select
              value={fontFamily}
              onValueChange={(value) => {
                const option = CHAT_FONT_OPTIONS.find((candidate) => candidate.value === value);
                if (option) setFontFamily(option.value);
              }}
            >
              <SelectTrigger
                id="settings-chat-font"
                style={{ fontFamily: selectedFontOption?.cssFontFamily }}
              >
                <SelectValue placeholder="Select a font" />
              </SelectTrigger>
              <SelectContent>
                {CHAT_FONT_OPTIONS.map((option) => (
                  <SelectItem
                    key={option.value}
                    value={option.value}
                    style={{ fontFamily: option.cssFontFamily }}
                  >
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {selectedFontOption && (
              <p className="text-xs leading-relaxed text-muted-foreground">
                {selectedFontOption.description}
              </p>
            )}
          </div>

          <div className="space-y-3">
            <div className="flex items-center justify-between gap-3">
              <Label htmlFor="settings-chat-font-size">Text size</Label>
              <output
                htmlFor="settings-chat-font-size"
                className="text-sm tabular-nums text-muted-foreground"
              >
                {fontSize}px
              </output>
            </div>
            <input
              id="settings-chat-font-size"
              type="range"
              min={CHAT_FONT_SIZE_MIN}
              max={CHAT_FONT_SIZE_MAX}
              step={CHAT_FONT_SIZE_STEP}
              value={fontSize}
              onChange={(event) => setFontSize(Number(event.currentTarget.value))}
              className="h-2 w-full cursor-pointer accent-primary"
              aria-valuetext={`${fontSize} pixels`}
            />
            <div className="flex items-center justify-between text-xs text-muted-foreground">
              <span>{CHAT_FONT_SIZE_MIN}px</span>
              <span>{CHAT_FONT_SIZE_MAX}px</span>
            </div>
          </div>

          <div
            className="rounded-lg border border-border/70 bg-muted/30 p-4"
            role="group"
            aria-labelledby="settings-chat-preview-label"
          >
            <p
              id="settings-chat-preview-label"
              className="text-xs font-medium uppercase tracking-wide text-muted-foreground"
            >
              Live preview
            </p>
            <div className="chat-typography mt-3 space-y-3">
              <div className="max-w-[88%] rounded-lg bg-background px-3 py-2 shadow-sm">
                <p className="mb-1 text-[0.75em] font-semibold text-muted-foreground">Assistant</p>
                <p className="leading-[1.65] tracking-[0.1px]">
                  Clear, comfortable text makes longer conversations easier to follow.
                </p>
              </div>
              <div className="ml-auto max-w-[88%] rounded-lg border border-border bg-muted px-3 py-2">
                <p className="mb-1 text-[0.75em] font-semibold text-muted-foreground">You</p>
                <p className="leading-[1.65] tracking-[0.1px]">This size feels just right.</p>
              </div>
            </div>
          </div>

          <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
            <div className="space-y-1">
              <p className="text-xs leading-relaxed text-muted-foreground">
                Applies to messages, reasoning, and tool activity in chats. Code stays monospace for
                readability.
              </p>
              <p className="text-xs leading-relaxed text-muted-foreground">
                Changes are saved automatically on this device.
              </p>
            </div>
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={resetTypography}
              disabled={!hasCustomTypography}
              className="shrink-0 gap-2"
            >
              <RotateCcw className="h-4 w-4" aria-hidden="true" />
              Reset chat appearance
            </Button>
          </div>
        </div>
      </SettingsSection>
      <SettingsSection
        title="Text-to-speech"
        description="Choose how Maple reads assistant responses aloud on this device."
      >
        <div className="space-y-6">
          <div className="grid gap-2">
            <Label htmlFor="settings-tts-voice">Voice accent</Label>
            <Select
              value={voice}
              onValueChange={(value) => {
                if (isVoxtralTTSVoice(value)) {
                  setVoice(value);
                }
              }}
            >
              <SelectTrigger id="settings-tts-voice">
                <SelectValue placeholder="Select a voice" />
              </SelectTrigger>
              <SelectContent>
                {TTS_VOICE_GROUPS.map((group) => (
                  <SelectGroup key={group}>
                    <SelectLabel>{group}</SelectLabel>
                    {VOXTRAL_TTS_VOICE_OPTIONS.filter((option) => option.group === group).map(
                      (option) => (
                        <SelectItem key={option.value} value={option.value}>
                          {option.label}
                        </SelectItem>
                      )
                    )}
                  </SelectGroup>
                ))}
              </SelectContent>
            </Select>
            <p className="text-xs leading-relaxed text-muted-foreground">
              Any voice can read any supported language; your text determines the spoken language.
              Matching the voice accent to the text generally sounds most natural.
            </p>
          </div>

          <div className="space-y-3">
            <div className="flex items-center justify-between gap-3">
              <Label htmlFor="settings-tts-speech-speed">Speech speed</Label>
              <output
                htmlFor="settings-tts-speech-speed"
                className="text-sm tabular-nums text-muted-foreground"
              >
                {formatPlaybackSpeed(playbackSpeed)}
              </output>
            </div>
            <input
              id="settings-tts-speech-speed"
              type="range"
              min={TTS_MIN_PLAYBACK_SPEED}
              max={TTS_MAX_PLAYBACK_SPEED}
              step={TTS_PLAYBACK_SPEED_STEP}
              value={playbackSpeed}
              onChange={(event) => setPlaybackSpeed(Number(event.currentTarget.value))}
              className="h-2 w-full cursor-pointer accent-primary"
              aria-valuetext={formatPlaybackSpeed(playbackSpeed)}
            />
            <div className="flex items-center justify-between text-xs text-muted-foreground">
              <span>{formatPlaybackSpeed(TTS_MIN_PLAYBACK_SPEED)}</span>
              <span>{formatPlaybackSpeed(TTS_MAX_PLAYBACK_SPEED)}</span>
            </div>
            <div className="flex items-center justify-between gap-4">
              <p className="text-xs leading-relaxed text-muted-foreground">
                Speech is generated at this speed to preserve the selected voice&apos;s pitch.
              </p>
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={resetPlaybackSpeed}
                disabled={!hasCustomPlaybackSpeed}
                className="shrink-0 gap-2"
              >
                <RotateCcw className="h-4 w-4" aria-hidden="true" />
                Reset
              </Button>
            </div>
          </div>

          <p className="text-xs leading-relaxed text-muted-foreground">
            Changes are saved automatically on this device.
          </p>
        </div>
      </SettingsSection>
    </SettingsPage>
  );
}
