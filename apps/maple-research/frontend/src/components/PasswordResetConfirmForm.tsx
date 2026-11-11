import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { AlertDestructive } from "@/components/AlertDestructive";
import { useOpenSecret } from "@mapleai/sdk";
import { useNavigate } from "@tanstack/react-router";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Info, Loader2 } from "lucide-react";

interface PasswordResetConfirmFormProps {
  email: string;
  secret: string;
}

export function PasswordResetConfirmForm({ email, secret }: PasswordResetConfirmFormProps) {
  const [code, setCode] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState(false);
  const [redirecting, setRedirecting] = useState(false);
  const [showCodeHelp, setShowCodeHelp] = useState(false);
  const navigate = useNavigate();
  const os = useOpenSecret();

  useEffect(() => {
    if (success) {
      setRedirecting(true);
      const timer = setTimeout(() => {
        navigate({ to: "/login" });
      }, 3000);
      return () => clearTimeout(timer);
    }
  }, [success, navigate]);

  useEffect(() => {
    if (success || code) {
      setShowCodeHelp(false);
      return;
    }

    const timer = setTimeout(() => {
      setShowCodeHelp(true);
    }, 30000);

    return () => clearTimeout(timer);
  }, [success, code]);

  const handleSubmit = async (e: React.FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    setIsLoading(true);
    setError(null);

    if (newPassword !== confirmPassword) {
      setError("Passwords do not match");
      setIsLoading(false);
      return;
    }

    try {
      await os.confirmPasswordReset(email, code, secret, newPassword);
      setSuccess(true);
    } catch (err) {
      console.error("Failed to reset password:", err);
      setError("Failed to reset password. Please try again.");
    } finally {
      setIsLoading(false);
    }
  };

  if (success) {
    return (
      <Card className="bg-card/70 backdrop-blur-sm mx-auto max-w-[45rem]">
        <CardHeader>
          <CardTitle className="text-xl">Password Reset Successful</CardTitle>
          <CardDescription>You can now log in with your new password.</CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col items-center">
          {redirecting ? (
            <>
              <Loader2 className="h-8 w-8 animate-spin mb-2" />
              <p>Redirecting to login page...</p>
            </>
          ) : (
            <p>You will be redirected to the login page in a few seconds.</p>
          )}
        </CardContent>
      </Card>
    );
  }

  return (
    <Card className="bg-card/70 backdrop-blur-sm mx-auto max-w-[45rem]">
      <CardHeader>
        <CardTitle className="text-xl">Confirm Password Reset</CardTitle>
        <CardDescription>Enter the reset code and your new password.</CardDescription>
      </CardHeader>
      <CardContent>
        <form onSubmit={handleSubmit}>
          <div className="grid gap-4">
            {showCodeHelp && (
              <Alert>
                <Info className="h-4 w-4" aria-hidden="true" />
                <AlertTitle>No code yet?</AlertTitle>
                <AlertDescription>
                  If no code arrives after a few minutes, this account might not use password login.
                  Try signing in with Apple, Google, or GitHub, or check spam before requesting
                  another reset.
                </AlertDescription>
              </Alert>
            )}
            <div className="grid gap-2">
              <Label htmlFor="code">Reset Code</Label>
              <Input
                id="code"
                value={code}
                onChange={(e) => setCode(e.target.value)}
                required
                pattern="[A-Z0-9]{8}|[A-Z0-9]{32}"
                title="8- or 32-character alphanumeric code"
              />
            </div>
            <div className="grid gap-2">
              <Label htmlFor="newPassword">New Password</Label>
              <Input
                id="newPassword"
                type="password"
                value={newPassword}
                onChange={(e) => setNewPassword(e.target.value)}
                required
                minLength={8}
              />
            </div>
            <div className="grid gap-2">
              <Label htmlFor="confirmPassword">Confirm New Password</Label>
              <Input
                id="confirmPassword"
                type="password"
                value={confirmPassword}
                onChange={(e) => setConfirmPassword(e.target.value)}
                required
                minLength={8}
              />
            </div>
            {error && <AlertDestructive title="Error" description={error} />}
            <Button type="submit" className="w-full" disabled={isLoading}>
              {isLoading ? "Resetting..." : "Reset Password"}
            </Button>
          </div>
        </form>
      </CardContent>
    </Card>
  );
}
