import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { MARKETING_DOWNLOADS_URL } from "@/config/domains";

/**
 * Shown instead of the application when a Maple release from before
 * Transport V2 opens the hosted sign-in page. The backend no longer serves
 * that release, so the only way forward is a current build.
 */
export function DesktopSignInUpdateRequired() {
  return (
    <Card className="max-w-md mx-auto mt-20">
      <CardHeader>
        <CardTitle>Update Maple to sign in</CardTitle>
      </CardHeader>
      <CardContent>
        <p className="mb-4">
          This version of Maple is no longer supported and can&apos;t sign in. Install the latest
          version, then sign in again.
        </p>
        <Button asChild>
          <a href={MARKETING_DOWNLOADS_URL}>Download Maple</a>
        </Button>
      </CardContent>
    </Card>
  );
}
