import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode
} from "react";
import { useLocation, useRouter } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { ProjectDetailView } from "@/components/ProjectDetailView";
import { UnifiedChat } from "@/components/UnifiedChat";
import {
  PersistentHomeNavigationContext,
  useAccountSidebarOpenState
} from "@/contexts/PersistentHomeNavigationContext";

const TRANSIENT_HOME_SEARCH_PARAMS = ["team_setup", "credits_success", "api_settings"];

type HomeSelection = {
  projectId: string | null;
  hasConversationId: boolean;
};

function readHomeSelection(): HomeSelection {
  if (typeof window === "undefined" || window.location.pathname !== "/") {
    return { projectId: null, hasConversationId: false };
  }

  const search = new URLSearchParams(window.location.search);
  return {
    projectId: search.get("project_id"),
    hasConversationId: search.has("conversation_id")
  };
}

function readSafeHomeHref(): string {
  if (typeof window === "undefined" || window.location.pathname !== "/") {
    return "/";
  }

  const search = new URLSearchParams(window.location.search);
  for (const key of TRANSIENT_HOME_SEARCH_PARAMS) {
    search.delete(key);
  }

  const searchString = search.toString();
  return `/${searchString ? `?${searchString}` : ""}${window.location.hash}`;
}

export function PersistentHomeNavigationProvider({ children }: { children: ReactNode }) {
  const location = useLocation();
  const router = useRouter();
  const os = useOpenSecret();
  const userId = os.auth.user?.user.id ?? null;
  const initialHomeHref = readSafeHomeHref();
  const homeHrefRef = useRef(initialHomeHref);
  const homeHrefUserIdRef = useRef(userId);
  const skipNextHomeCaptureRef = useRef(false);
  const [sidebarOpen, setSidebarOpen] = useAccountSidebarOpenState(userId);

  const captureHomeHref = useCallback(() => {
    if (!userId || window.location.pathname !== "/") return;

    const nextHomeHref = readSafeHomeHref();
    homeHrefRef.current = nextHomeHref;
  }, [userId]);

  useLayoutEffect(() => {
    if (homeHrefUserIdRef.current === userId) return;

    // Conversation and project IDs are account-scoped; never carry a home snapshot across users.
    homeHrefUserIdRef.current = userId;
    skipNextHomeCaptureRef.current = true;
    homeHrefRef.current = "/";
  }, [userId]);

  // TanStack navigations update location.href. Maple also updates the home URL with the native
  // History API, so the app events below keep the snapshot exact for those transitions too.
  useLayoutEffect(() => {
    if (skipNextHomeCaptureRef.current) {
      skipNextHomeCaptureRef.current = false;
      return;
    }
    captureHomeHref();
  }, [captureHomeHref, location.href]);

  useEffect(() => {
    const events = [
      "conversationcreated",
      "conversationselected",
      "projectselected",
      "newchat",
      "popstate"
    ] as const;

    for (const event of events) {
      window.addEventListener(event, captureHomeHref);
    }

    return () => {
      for (const event of events) {
        window.removeEventListener(event, captureHomeHref);
      }
    };
  }, [captureHomeHref]);

  const returnToHome = useCallback(
    ({ replace = true }: { replace?: boolean } = {}) => {
      const href = homeHrefRef.current;
      if (replace) {
        router.history.replace(href);
      } else {
        router.history.push(href);
      }
    },
    [router]
  );

  const value = useMemo(
    () => ({
      returnToHome,
      sidebarOpen,
      setSidebarOpen
    }),
    [returnToHome, setSidebarOpen, sidebarOpen]
  );

  return (
    <PersistentHomeNavigationContext.Provider value={value}>
      {children}
    </PersistentHomeNavigationContext.Provider>
  );
}

export function AuthenticatedHomeContent({
  homeLocationHref
}: {
  homeLocationHref: string | null;
}) {
  const [selection, setSelection] = useState<HomeSelection>(readHomeSelection);

  const syncFromHomeLocation = useCallback(() => {
    if (window.location.pathname !== "/") return;

    const nextSelection = readHomeSelection();
    setSelection((current) =>
      current.projectId === nextSelection.projectId &&
      current.hasConversationId === nextSelection.hasConversationId
        ? current
        : nextSelection
    );
  }, []);

  useEffect(() => {
    if (homeLocationHref !== null) {
      syncFromHomeLocation();
    }
  }, [homeLocationHref, syncFromHomeLocation]);

  useEffect(() => {
    const events = ["projectselected", "conversationselected", "newchat", "popstate"] as const;

    for (const event of events) {
      window.addEventListener(event, syncFromHomeLocation);
    }

    return () => {
      for (const event of events) {
        window.removeEventListener(event, syncFromHomeLocation);
      }
    };
  }, [syncFromHomeLocation]);

  if (selection.projectId && !selection.hasConversationId) {
    return <ProjectDetailView projectId={selection.projectId} />;
  }

  return <UnifiedChat isVisible={homeLocationHref !== null} />;
}
