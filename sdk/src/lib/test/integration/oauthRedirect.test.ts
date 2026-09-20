import { expect, test } from "bun:test";
import { encode } from "@stablelib/base64";
import {
  getApiPcrConfig,
  getApiUrl,
  initiateGitHubAuth,
  initiateGoogleAuth,
  type GithubAuthResponse
} from "../../api";
import {
  createOrganization,
  createProject,
  createProjectSecret,
  deleteOrganization,
  deleteProject,
  getOAuthSettings,
  getPlatformApiUrl,
  getPlatformPcrConfig,
  platformLogin,
  platformRegister,
  setPlatformApiUrl,
  updateOAuthSettings,
  type OAuthProviderSettings,
  type OAuthSettings
} from "../../platformApi";
import { transportV2Runtime } from "../../transportV2/runtime";

const providers = [
  {
    name: "github",
    initiate: initiateGitHubAuth,
    authorizationEndpoint: "https://github.com/login/oauth/authorize",
    secretKey: "GITHUB_OAUTH_SECRET"
  },
  {
    name: "google",
    initiate: initiateGoogleAuth,
    authorizationEndpoint: "https://accounts.google.com/o/oauth2/v2/auth",
    secretKey: "GOOGLE_OAUTH_SECRET"
  }
] as const;

type Provider = (typeof providers)[number];
type AdditionalUrlsUpdate = "registered" | "omitted" | "null" | "empty";

function defaultRedirect(provider: Provider): string {
  return `https://app.example.test/auth/${provider.name}/callback`;
}

function additionalRedirect(provider: Provider): string {
  return `https://auth.example.test/auth/${provider.name}/callback?channel=hosted`;
}

function providerSettings(
  provider: Provider,
  additionalUrls: AdditionalUrlsUpdate
): OAuthProviderSettings {
  return {
    client_id: `sdk-fixture-${provider.name}`,
    redirect_url: defaultRedirect(provider),
    ...(additionalUrls === "omitted"
      ? {}
      : {
          additional_redirect_urls:
            additionalUrls === "null"
              ? null
              : additionalUrls === "empty"
                ? []
                : [additionalRedirect(provider)]
        })
  };
}

function oauthSettings(additionalUrls: AdditionalUrlsUpdate): OAuthSettings {
  return {
    github_oauth_enabled: true,
    google_oauth_enabled: true,
    apple_oauth_enabled: false,
    github_oauth_settings: providerSettings(providers[0], additionalUrls),
    google_oauth_settings: providerSettings(providers[1], additionalUrls)
  };
}

function snapshotStorage(storage: Storage): [string, string][] {
  const entries: [string, string][] = [];
  for (let index = 0; index < storage.length; index += 1) {
    const key = storage.key(index);
    if (key !== null) {
      const value = storage.getItem(key);
      if (value !== null) entries.push([key, value]);
    }
  }
  return entries;
}

function restoreStorage(storage: Storage, entries: [string, string][]): void {
  storage.clear();
  for (const [key, value] of entries) storage.setItem(key, value);
}

async function loginFixtureDeveloper(): Promise<void> {
  const email = process.env.VITE_TEST_DEVELOPER_EMAIL;
  const password = process.env.VITE_TEST_DEVELOPER_PASSWORD;
  const name = process.env.VITE_TEST_DEVELOPER_NAME;
  const inviteCode = process.env.VITE_TEST_DEVELOPER_INVITE_CODE;
  if (!email || !password || !name || !inviteCode) {
    throw new Error("Disposable developer credentials and invite code are required.");
  }

  try {
    await platformLogin(email, password);
  } catch {
    try {
      await platformRegister(email, password, inviteCode, name);
    } catch (error) {
      if (
        error instanceof Error &&
        /Email already registered|User already exists/u.test(error.message)
      ) {
        await platformLogin(email, password);
      } else {
        throw error;
      }
    }
  }
}

async function fixtureStage<T>(stage: string, operation: () => Promise<T>): Promise<T> {
  try {
    return await operation();
  } catch (error) {
    // Labels contain only fixed fixture operation/provider names, never request data.
    console.error(`OAuth redirect integration failed during ${stage}.`);
    throw error;
  }
}

function expectAuthorization(
  provider: Provider,
  response: GithubAuthResponse,
  redirectUrl: string
): void {
  // Inspect the URL only. Following it or exchanging a code would contact a provider.
  const authorization = new URL(response.auth_url);
  expect(`${authorization.origin}${authorization.pathname}`).toBe(provider.authorizationEndpoint);
  expect(authorization.searchParams.get("client_id")).toBe(`sdk-fixture-${provider.name}`);
  expect(authorization.searchParams.get("redirect_uri")).toBe(redirectUrl);
  expect(authorization.searchParams.get("response_type")).toBe("code");
  expect(authorization.searchParams.get("code_challenge_method")).toBe("S256");
  expect(authorization.searchParams.get("code_challenge")).toBeTruthy();
  // V2's opaque state retains the existing SDK csrf_token response property.
  expect(response.csrf_token).toBeTruthy();
  expect(authorization.searchParams.get("state")).toBe(response.csrf_token);
}

test("encrypted OAuth callback selection and settings preserve legacy defaults", async () => {
  const apiUrl = getApiUrl();
  if (
    !["127.0.0.1", "localhost", "[::1]"].includes(new URL(apiUrl).hostname) ||
    getApiPcrConfig().environment !== "development"
  ) {
    throw new Error(
      "OAuth redirect integration requires a disposable loopback development backend."
    );
  }
  const originalPlatformApiUrl = getPlatformApiUrl();
  const originalPlatformPcrConfig = getPlatformPcrConfig();
  const originalLocalStorage = snapshotStorage(localStorage);
  const originalSessionStorage = snapshotStorage(sessionStorage);

  localStorage.clear();
  sessionStorage.clear();
  transportV2Runtime.clear(apiUrl);
  setPlatformApiUrl(apiUrl, getApiPcrConfig());

  try {
    await fixtureStage("developer login", loginFixtureDeveloper);
    const marker = crypto.randomUUID();
    const organization = await fixtureStage("organization creation", () =>
      createOrganization(`SDK OAuth ${marker}`)
    );
    try {
      // Both organization and project names must fit the backend's 50-character limit.
      const project = await fixtureStage("project creation", () =>
        createProject(organization.id, `OAuth ${marker}`)
      );
      try {
        for (const provider of providers) {
          await fixtureStage(`${provider.name} secret creation`, () =>
            createProjectSecret(
              organization.id,
              project.id,
              provider.secretKey,
              encode(new TextEncoder().encode(`dummy-${provider.name}-secret-${marker}`))
            )
          );
        }
        await fixtureStage("initial OAuth settings", () =>
          updateOAuthSettings(organization.id, project.id, oauthSettings("registered"))
        );

        for (const provider of providers) {
          // The original one-argument API and an explicit default select the same URL.
          expectAuthorization(
            provider,
            await provider.initiate(project.client_id),
            defaultRedirect(provider)
          );
          for (const redirect of [defaultRedirect(provider), additionalRedirect(provider)]) {
            expectAuthorization(
              provider,
              await provider.initiate(project.client_id, undefined, redirect),
              redirect
            );
          }
          const otherProvider = providers.find((candidate) => candidate.name !== provider.name)!;
          for (const unlisted of [
            "https://unlisted.example.test/callback",
            additionalRedirect(otherProvider)
          ]) {
            await expect(
              provider.initiate(project.client_id, undefined, unlisted)
            ).rejects.toMatchObject({ status: 400 });
          }
        }

        for (const update of ["omitted", "null", "empty"] as const) {
          const expected = oauthSettings(update === "empty" ? "empty" : "registered");
          expect(
            await fixtureStage(`${update} OAuth settings update`, () =>
              updateOAuthSettings(organization.id, project.id, oauthSettings(update))
            )
          ).toMatchObject(expected);
          expect(await getOAuthSettings(organization.id, project.id)).toMatchObject(expected);
          for (const provider of providers) {
            if (update === "empty") {
              await expect(
                provider.initiate(project.client_id, undefined, additionalRedirect(provider))
              ).rejects.toMatchObject({ status: 400 });
              expectAuthorization(
                provider,
                await provider.initiate(project.client_id),
                defaultRedirect(provider)
              );
            } else {
              expectAuthorization(
                provider,
                await provider.initiate(project.client_id, undefined, additionalRedirect(provider)),
                additionalRedirect(provider)
              );
            }
          }
        }
      } finally {
        await deleteProject(organization.id, project.id);
      }
    } finally {
      await deleteOrganization(organization.id);
    }
  } finally {
    transportV2Runtime.clear(apiUrl);
    setPlatformApiUrl(originalPlatformApiUrl, originalPlatformPcrConfig);
    restoreStorage(localStorage, originalLocalStorage);
    restoreStorage(sessionStorage, originalSessionStorage);
  }
});
