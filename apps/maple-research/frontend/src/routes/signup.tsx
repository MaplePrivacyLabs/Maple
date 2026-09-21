import { createFileRoute, Link, useLocation, useNavigate, useRouter } from "@tanstack/react-router";
import { useOpenSecret, type LoginResponse } from "@mapleai/sdk";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { AlertDestructive } from "@/components/AlertDestructive";
import { Loader2, Github, Mail } from "lucide-react";
import { Google } from "@/components/icons/Google";
import { Apple } from "@/components/icons/Apple";
import { AuthMain } from "@/components/AuthMain";
import { invoke } from "@tauri-apps/api/core";
import { v4 as uuidv4 } from "uuid";
import type { AppleCredential } from "@/types/apple-sign-in";
import { sha256 } from "@noble/hashes/sha256";
import { bytesToHex } from "@noble/hashes/utils";
import { AppleAuthProvider } from "@/components/AppleAuthProvider";
import { getBillingService } from "@/billing/billingService";
import { isIOS, isTauri } from "@/utils/platform";
import { GuestSignupWarningDialog } from "@/components/GuestSignupWarningDialog";
import { GuestCredentialsDialog } from "@/components/GuestCredentialsDialog";
import { UserCircle } from "lucide-react";
import { appUrl } from "@/config/domains";
import { useRouteMeta } from "@/utils/routeMeta";
import { getSafeInternalRedirect, navigateToSafeInternalRedirect } from "@/utils/internalRedirect";
import { shouldRedirectAuthenticatedSignup } from "@/utils/signupRedirect";
import { startNativeOAuth } from "@/services/nativeOAuthAttempt";
import { clearDesktopOAuthTransport } from "@/services/desktopOAuthTransport";

type SignupSearchParams = {
  next?: string;
  selected_plan?: string;
  code?: string;
};

export const Route = createFileRoute("/signup")({
  component: SignupPage,
  validateSearch: (search: Record<string, unknown>): SignupSearchParams => ({
    next: getSafeInternalRedirect(search.next),
    selected_plan: typeof search.selected_plan === "string" ? search.selected_plan : undefined,
    code: typeof search.code === "string" ? search.code : undefined
  })
});

type SignUpMethod = "email" | "github" | "google" | "apple" | "guest" | null;
const signupCanonicalUrl = appUrl("/signup");

function SignupPage() {
  useRouteMeta({
    title: "Sign Up | Maple Research",
    description: "Create a Maple Research account for your private AI workspace.",
    canonicalUrl: signupCanonicalUrl
  });

  const navigate = useNavigate();
  const router = useRouter();
  const location = useLocation();
  const os = useOpenSecret();
  const { next, selected_plan, code } = Route.useSearch();
  const [signUpMethod, setSignUpMethod] = useState<SignUpMethod>(null);
  const [error, setError] = useState<string | null>(null);
  const [isLoading, setIsLoading] = useState(false);
  const [showGuestWarning, setShowGuestWarning] = useState(false);
  const [showGuestCredentials, setShowGuestCredentials] = useState(false);
  const [guestUuid, setGuestUuid] = useState<string | null>(null);

  // Use platform detection functions
  const isIOSPlatform = isIOS();
  const isTauriEnv = isTauri();

  // Redirect if already logged in (but not if we're showing guest credentials)
  useEffect(() => {
    if (
      shouldRedirectAuthenticatedSignup({
        isAuthenticated: !!os.auth.user,
        isGuestSignup: signUpMethod === "guest",
        isSignupRoute: location.pathname === "/signup",
        showGuestCredentials
      })
    ) {
      if (selected_plan) {
        navigate({
          to: "/pricing",
          search: { selected_plan }
        });
      } else if (next === "/redeem" && code) {
        navigate({
          to: "/redeem",
          search: { code }
        });
      } else {
        if (!navigateToSafeInternalRedirect(router.history, next)) {
          navigate({ to: "/" });
        }
      }
    }
  }, [
    os.auth.user,
    navigate,
    next,
    selected_plan,
    code,
    location.pathname,
    signUpMethod,
    showGuestCredentials,
    router
  ]);

  const handleSubmit = async (e: React.FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    setIsLoading(true);
    setError(null);
    const formData = new FormData(e.currentTarget);
    const email = formData.get("email") as string;
    const password = formData.get("password") as string;

    try {
      await os.signUp(email, password, "", "ANON");
      // Clear any existing billing token to prevent session mixing
      try {
        getBillingService().clearToken();
      } catch (billingError) {
        console.warn("Failed to clear billing token:", billingError);
      }
      setTimeout(() => {
        if (selected_plan) {
          navigate({
            to: "/pricing",
            search: { selected_plan }
          });
        } else if (next === "/redeem" && code) {
          navigate({
            to: "/redeem",
            search: { code }
          });
        } else {
          if (!navigateToSafeInternalRedirect(router.history, next)) {
            navigate({ to: "/" });
          }
        }
        window.scrollTo(0, 0);
      }, 100);
    } catch (error) {
      console.error(error);
      if (error instanceof Error) {
        setError(`${error.message}`);
      } else {
        setError("Something went wrong. Please try again.");
      }
    } finally {
      setIsLoading(false);
    }
  };

  const handleGitHubSignup = async () => {
    try {
      console.log("[OAuth] Using", isTauriEnv ? "Tauri" : "web", "flow");

      if (isTauriEnv) {
        await startNativeOAuth("github", import.meta.env.VITE_OPEN_SECRET_API_URL, {
          selectedPlan: selected_plan,
          next,
          redemptionCode: code
        });
      } else {
        // Web flow remains unchanged
        clearDesktopOAuthTransport();
        const { auth_url } = await os.initiateGitHubAuth("");
        sessionStorage.removeItem("selected_plan");
        if (selected_plan) {
          sessionStorage.setItem("selected_plan", selected_plan);
        }
        if (code) {
          sessionStorage.setItem("redeem_code", code);
        }
        sessionStorage.removeItem("post_auth_redirect");
        if (next) {
          sessionStorage.setItem("post_auth_redirect", next);
        }
        window.location.href = auth_url;
      }
    } catch (error) {
      console.error("Failed to initiate GitHub signup:", error);
      setError("Failed to initiate GitHub signup. Please try again.");
    }
  };

  const handleGoogleSignup = async () => {
    try {
      console.log("[OAuth] Using", isTauriEnv ? "Tauri" : "web", "flow");

      if (isTauriEnv) {
        await startNativeOAuth("google", import.meta.env.VITE_OPEN_SECRET_API_URL, {
          selectedPlan: selected_plan,
          next,
          redemptionCode: code
        });
      } else {
        // Web flow remains unchanged
        clearDesktopOAuthTransport();
        const { auth_url } = await os.initiateGoogleAuth("");
        sessionStorage.removeItem("selected_plan");
        if (selected_plan) {
          sessionStorage.setItem("selected_plan", selected_plan);
        }
        if (code) {
          sessionStorage.setItem("redeem_code", code);
        }
        sessionStorage.removeItem("post_auth_redirect");
        if (next) {
          sessionStorage.setItem("post_auth_redirect", next);
        }
        window.location.href = auth_url;
      }
    } catch (error) {
      console.error("Failed to initiate Google signup:", error);
      setError("Failed to initiate Google signup. Please try again.");
    }
  };

  const handleGuestWarningAccept = () => {
    setShowGuestWarning(false);
    setSignUpMethod("guest");
  };

  const handleGuestSubmit = async (e: React.FormEvent<HTMLFormElement>) => {
    e.preventDefault();

    // Blur the active element (password input) to prevent conflicts with password managers
    // (like Bitwarden) that might try to show a popup/overlay when Enter is pressed,
    // which can conflict with the GuestCredentialsDialog.
    if (document.activeElement instanceof HTMLElement) {
      document.activeElement.blur();
    }

    setIsLoading(true);
    setError(null);
    const formData = new FormData(e.currentTarget);
    const password = formData.get("password") as string;

    try {
      const response: LoginResponse = await os.signUpGuest(password, "");
      setGuestUuid(response.id);
      // Clear any existing billing token to prevent session mixing
      try {
        getBillingService().clearToken();
      } catch (billingError) {
        console.warn("Failed to clear billing token:", billingError);
      }
      // Show credentials dialog
      setShowGuestCredentials(true);
    } catch (error) {
      console.error(error);
      if (error instanceof Error) {
        setError(`${error.message}`);
      } else {
        setError("Something went wrong. Please try again.");
      }
    } finally {
      setIsLoading(false);
    }
  };

  const handleGuestCredentialsContinue = () => {
    setShowGuestCredentials(false);
    // Navigate to pricing page (guests need to pay immediately)
    navigate({ to: "/pricing" });
  };

  const handleAppleSignup = async () => {
    try {
      if (isTauriEnv && isIOSPlatform) {
        // Native iOS implementation using Apple Sign In plugin
        console.log("[OAuth] Initiating native Sign in with Apple for iOS");

        try {
          // Generate random UUIDs for state and nonce
          const state = uuidv4();
          const rawNonce = uuidv4();

          // SHA-256 hash the nonce before sending to Apple
          // Apple requires the nonce to be hashed with SHA-256
          const hashedNonce = bytesToHex(sha256(new TextEncoder().encode(rawNonce)));

          // Invoke the Apple Sign in plugin
          // This will show the native Apple authentication UI
          const result = await invoke<AppleCredential>(
            "plugin:sign-in-with-apple|get_apple_id_credential",
            {
              payload: {
                scope: ["email", "fullName"],
                state,
                nonce: hashedNonce, // Send the hashed nonce to Apple
                // Disable debug mode in production
                options: {
                  debug: false
                }
              }
            }
          );

          console.log("[OAuth] Apple Sign-In completed");

          // Format the response for the API
          const appleUser = {
            user_identifier: result.user,
            identity_token: result.identityToken,
            email: result.email,
            given_name: result.fullName?.givenName,
            family_name: result.fullName?.familyName,
            nonce: rawNonce // Pass the original raw nonce to backend
          };

          // Send to backend via SDK
          try {
            await os.handleAppleNativeSignIn(appleUser, "");
            // Clear any existing billing token to prevent session mixing
            try {
              getBillingService().clearToken();
            } catch (billingError) {
              console.warn("Failed to clear billing token:", billingError);
            }
            // Redirect after successful signup
            if (selected_plan) {
              navigate({
                to: "/pricing",
                search: { selected_plan }
              });
            } else if (next === "/redeem" && code) {
              navigate({
                to: "/redeem",
                search: { code }
              });
            } else {
              if (!navigateToSafeInternalRedirect(router.history, next)) {
                navigate({ to: "/" });
              }
            }
          } catch (backendError) {
            console.error("[OAuth] Backend processing failed:", backendError);
            setError(
              backendError instanceof Error
                ? backendError.message
                : "Failed to process Apple authentication"
            );
          }
        } catch (error) {
          console.error("[OAuth] Failed to authenticate with Apple:", error);
          const errorMessage =
            error instanceof Error
              ? `Apple Sign In error: ${error.message}`
              : "Failed to authenticate with Apple. Please try again.";
          setError(errorMessage);
        }
      } else if (isTauriEnv) {
        await startNativeOAuth("apple", import.meta.env.VITE_OPEN_SECRET_API_URL, {
          selectedPlan: selected_plan,
          next,
          redemptionCode: code
        });
      } else {
        // Web flow - use AppleAuthProvider component which will initiate the flow
        console.log("[OAuth] Using web flow for Apple Sign In (Web only)");
        // Store redemption code in sessionStorage for AppleAuthProvider
        if (code) {
          sessionStorage.setItem("redeem_code", code);
        }
        // The AppleAuthProvider component handles everything for web
        // It will be triggered by the onClick event on the button
      }
    } catch (error) {
      console.error("Failed to initiate Apple signup:", error);
      setError("Failed to initiate Apple signup. Please try again.");
    }
  };

  if (!signUpMethod) {
    return (
      <AuthMain title="Sign Up" description="Choose your preferred sign-up method">
        {error && <AlertDestructive title="Note" description={error} />}
        <Button onClick={() => setSignUpMethod("email")} variant="primary" className="w-full">
          <Mail className="mr-2 h-4 w-4" />
          Sign up with Email
        </Button>
        <Button
          onClick={handleGitHubSignup}
          variant="outline"
          className="w-full bg-white/40 dark:bg-white/0"
        >
          <Github className="mr-2 h-4 w-4" />
          Sign up with GitHub
        </Button>
        <Button
          onClick={handleGoogleSignup}
          variant="outline"
          className="w-full bg-white/40 dark:bg-white/0"
        >
          <Google className="mr-2 h-4 w-4" />
          Sign up with Google
        </Button>
        {isTauriEnv ? (
          <Button
            onClick={handleAppleSignup}
            variant="outline"
            className="w-full bg-white/40 dark:bg-white/0"
          >
            <Apple className="mr-2 h-4 w-4" />
            Sign up with Apple
          </Button>
        ) : (
          <AppleAuthProvider
            onError={(error) => setError(error.message)}
            redirectAfterLogin={(plan) => {
              if (plan) {
                navigate({ to: "/pricing", search: { selected_plan: plan } });
              } else {
                if (!navigateToSafeInternalRedirect(router.history, next)) {
                  navigate({ to: "/" });
                }
              }
            }}
            selectedPlan={selected_plan}
            inviteCode=""
            buttonLabel="Sign up with Apple"
            buttonVariant="outline"
            className="w-full bg-white/40 dark:bg-white/0"
          />
        )}
        <Button
          onClick={() => setShowGuestWarning(true)}
          variant="outline"
          className="w-full bg-white/40 dark:bg-white/0"
        >
          <UserCircle className="mr-2 h-4 w-4" />
          Sign up as Anonymous
        </Button>
        <div className="text-center text-sm">
          Already have an account?{" "}
          <Link
            to="/login"
            search={next ? { next, ...(code && { code }) } : code ? { code } : undefined}
            className="underline"
          >
            Log In
          </Link>
        </div>
        <GuestSignupWarningDialog
          open={showGuestWarning}
          onOpenChange={setShowGuestWarning}
          onAccept={handleGuestWarningAccept}
        />
      </AuthMain>
    );
  }

  if (signUpMethod === "guest") {
    return (
      <>
        <form onSubmit={handleGuestSubmit}>
          <AuthMain title="Sign Up as Anonymous">
            {error && <AlertDestructive title="Error" description={error} />}
            <div className="grid gap-2">
              <Label htmlFor="password">Create Password</Label>
              <Input
                id="password"
                name="password"
                type="password"
                required
                minLength={8}
                autoComplete="new-password"
                placeholder="Create a strong password"
              />
              <p className="text-xs text-muted-foreground">
                You'll need this password along with your Account ID to sign in. Your Account ID
                will be shown in the next step.
              </p>
            </div>
            <Button type="submit" variant="primary" className="w-full" disabled={isLoading}>
              {isLoading ? (
                <>
                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                  Creating Account...
                </>
              ) : (
                "Create Anonymous Account"
              )}
            </Button>
            <Button
              type="button"
              variant="outline"
              onClick={() => setSignUpMethod(null)}
              className="w-full"
            >
              Back
            </Button>
          </AuthMain>
        </form>
        {guestUuid && (
          <GuestCredentialsDialog
            open={showGuestCredentials}
            onOpenChange={setShowGuestCredentials}
            uuid={guestUuid}
            onContinue={handleGuestCredentialsContinue}
          />
        )}
      </>
    );
  }

  return (
    <form onSubmit={handleSubmit}>
      <AuthMain title="Sign Up with Email">
        {error && <AlertDestructive title="Error" description={error} />}
        <div className="grid gap-2">
          <Label htmlFor="email">Email</Label>
          <Input
            id="email"
            name="email"
            type="email"
            placeholder="email@example.com"
            required
            autoComplete="email"
          />
        </div>
        <div className="grid gap-2">
          <Label htmlFor="password">Password</Label>
          <Input
            id="password"
            name="password"
            type="password"
            required
            minLength={8}
            autoComplete="new-password"
          />
        </div>
        <Button type="submit" variant="primary" className="w-full" disabled={isLoading}>
          {isLoading ? (
            <>
              <Loader2 className="mr-2 h-4 w-4 animate-spin" />
              Please wait
            </>
          ) : (
            "Create Account"
          )}
        </Button>
        <Button
          type="button"
          variant="outline"
          onClick={() => setSignUpMethod(null)}
          className="w-full"
        >
          Back
        </Button>
        <div className="text-center text-sm">
          Already have an account?{" "}
          <Link
            to="/login"
            search={next ? { next, ...(code && { code }) } : code ? { code } : undefined}
            className="underline"
          >
            Log In
          </Link>
        </div>
      </AuthMain>
    </form>
  );
}
