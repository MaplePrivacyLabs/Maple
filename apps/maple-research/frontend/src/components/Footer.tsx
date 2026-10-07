import { Github, Twitter, Mail } from "lucide-react";
import { Link } from "@tanstack/react-router";
import { DiscordIcon } from "./icons/DiscordIcon";
import { MARKETING_DOWNLOADS_URL } from "@/config/domains";
import { openExternalUrl } from "@/utils/openUrl";
import { isAndroid, isTauri } from "@/utils/platform";
import { GITHUB_REPOSITORY_URL } from "@/utils/githubRelease";

export function Footer() {
  const isTauriPlatform = isTauri();

  if (isAndroid()) {
    return (
      <footer className="flex flex-wrap justify-center gap-4 border-t py-8 text-sm text-muted-foreground">
        <Link to="/privacy">Privacy Policy</Link>
        <Link to="/terms">Terms of Service</Link>
        <a href="mailto:support@trymaple.ai">Contact support</a>
      </footer>
    );
  }

  return (
    <div className="w-full dark:bg-[hsl(var(--background))] bg-[hsl(var(--footer-bg))] py-16 border-t border-[hsl(var(--marketing-card-border))]">
      <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
        <div className="grid grid-cols-1 md:grid-cols-3 gap-8">
          <div className="flex flex-col gap-4">
            <img src="/maple-logo-dark.svg" alt="Maple" className="w-32 block dark:hidden" />
            <img src="/maple-logo.svg" alt="Maple" className="w-32 hidden dark:block" />
            <p className="text-[hsl(var(--marketing-text-muted))] font-light">
              Private AI chat with end-to-end encryption. Your conversations stay yours.
            </p>
            <div className="flex gap-5 text-[hsl(var(--marketing-text-muted))]">
              <a
                href="https://twitter.com/TryMapleAI"
                target="_blank"
                rel="noopener noreferrer"
                className="dark:hover:text-[hsl(var(--blue))] hover:text-[hsl(var(--purple))] transition-colors"
                aria-label="Twitter"
              >
                <Twitter className="h-5 w-5" />
              </a>
              <a
                href={GITHUB_REPOSITORY_URL}
                target="_blank"
                rel="noopener noreferrer"
                className="dark:hover:text-[hsl(var(--blue))] hover:text-[hsl(var(--purple))] transition-colors"
                aria-label="GitHub"
              >
                <Github className="h-5 w-5" />
              </a>
              <a
                href="https://discord.gg/ch2gjZAMGy"
                target="_blank"
                rel="noopener noreferrer"
                className="dark:hover:text-[hsl(var(--blue))] hover:text-[hsl(var(--purple))] transition-colors"
                aria-label="Discord"
              >
                <DiscordIcon className="h-5 w-5" />
              </a>
              <a
                href="mailto:support@trymaple.ai"
                className="dark:hover:text-[hsl(var(--blue))] hover:text-[hsl(var(--green))] transition-colors"
                aria-label="Email"
              >
                <Mail className="h-5 w-5" />
              </a>
            </div>
          </div>

          <div className="flex flex-col gap-4">
            <h3 className="text-foreground text-lg font-medium">Product</h3>
            <Link
              to="/pricing"
              className="text-[hsl(var(--marketing-text-muted))] hover:text-foreground transition-colors"
            >
              Pricing
            </Link>
            <a
              href={MARKETING_DOWNLOADS_URL}
              target="_blank"
              rel="noopener noreferrer"
              onClick={(event) => {
                if (!isTauriPlatform) return;
                event.preventDefault();
                void openExternalUrl(MARKETING_DOWNLOADS_URL);
              }}
              className="text-[hsl(var(--marketing-text-muted))] hover:text-foreground transition-colors"
            >
              Downloads
            </a>
            <Link
              to="/proof"
              className="text-[hsl(var(--marketing-text-muted))] hover:text-foreground transition-colors"
            >
              Security Proof
            </Link>
          </div>

          <div className="flex flex-col gap-4">
            <h3 className="text-foreground text-lg font-medium">Resources</h3>
            <a
              href="https://blog.trymaple.ai"
              target="_blank"
              rel="noopener noreferrer"
              className="text-[hsl(var(--marketing-text-muted))] hover:text-foreground transition-colors"
            >
              Blog
            </a>
            <a
              href="https://opensecret.cloud"
              target="_blank"
              rel="noopener noreferrer"
              className="text-[hsl(var(--marketing-text-muted))] hover:text-foreground transition-colors"
            >
              OpenSecret
            </a>
            <a
              href="https://discord.gg/ch2gjZAMGy"
              target="_blank"
              rel="noopener noreferrer"
              className="text-[hsl(var(--marketing-text-muted))] hover:text-foreground transition-colors"
            >
              Community
            </a>
            <Link
              to="/about"
              className="text-[hsl(var(--marketing-text-muted))] hover:text-foreground transition-colors"
            >
              About
            </Link>
            <Link
              to="/privacy"
              className="text-[hsl(var(--marketing-text-muted))] hover:text-foreground transition-colors"
            >
              Privacy Policy
            </Link>
            <Link
              to="/terms"
              className="text-[hsl(var(--marketing-text-muted))] hover:text-foreground transition-colors"
            >
              Terms of Service
            </Link>
          </div>
        </div>

        <div className="mt-12 pt-8 border-t border-[hsl(var(--marketing-card-border))] text-center">
          <div className="flex flex-col items-center gap-4">
            {/* Only show status iframe on web (doesn't work in Tauri builds) */}
            {!isTauriPlatform && (
              <iframe
                src="https://status.trymaple.ai/badge?theme=system"
                width="250"
                height="30"
                frameBorder="0"
                scrolling="no"
                sandbox="allow-scripts allow-popups allow-popups-to-escape-sandbox"
                style={{ colorScheme: "normal", marginLeft: "58px" }}
                title="BetterStack Status"
              />
            )}
            <p className="text-[hsl(var(--marketing-text-muted))]/50 font-light">
              © {new Date().getFullYear()} Maple Privacy Labs Inc. All rights reserved.
            </p>
          </div>
        </div>
      </div>
    </div>
  );
}
