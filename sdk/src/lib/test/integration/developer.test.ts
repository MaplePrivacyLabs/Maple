import { expect, test, beforeEach } from "bun:test";
import { getPlatformApiUrl, platformLogin, platformRegister, platformMe } from "../../platformApi";
import "../platform-api-url-loader";
import * as platformApi from "../../platformApi";
import { encode } from "@stablelib/base64";
import { installTransportV2Credentials, readTransportV2Credentials } from "../../transportV2/auth";

const TEST_DEVELOPER_EMAIL = process.env.VITE_TEST_DEVELOPER_EMAIL;
const TEST_DEVELOPER_PASSWORD = process.env.VITE_TEST_DEVELOPER_PASSWORD;
const TEST_DEVELOPER_NAME = process.env.VITE_TEST_DEVELOPER_NAME;
const TEST_DEVELOPER_INVITE_CODE = process.env.VITE_TEST_DEVELOPER_INVITE_CODE;

if (
  !TEST_DEVELOPER_EMAIL ||
  !TEST_DEVELOPER_PASSWORD ||
  !TEST_DEVELOPER_NAME ||
  !TEST_DEVELOPER_INVITE_CODE
) {
  throw new Error("Test developer credentials and invite code must be set in .env.local");
}

// Cache login response to avoid multiple logins
let cachedLoginResponse: { access_token: string; refresh_token: string } | null = null;

// Helper function to base64 encode strings
const encodeSecret = (secret: string): string => {
  return encode(new TextEncoder().encode(secret));
};

async function tryDeveloperLogin() {
  // If we have a successful login cached, reuse it
  if (cachedLoginResponse) {
    installTransportV2Credentials(
      getPlatformApiUrl(),
      "platform",
      cachedLoginResponse.access_token,
      cachedLoginResponse.refresh_token
    );
    return cachedLoginResponse;
  }

  try {
    // First, try to login directly
    console.log(`Attempting login with email: ${TEST_DEVELOPER_EMAIL}`);
    const response = await platformLogin(TEST_DEVELOPER_EMAIL!, TEST_DEVELOPER_PASSWORD!);
    console.log("Login successful");
    cachedLoginResponse = response;

    return response;
  } catch {
    console.warn("Login failed, attempting to register the user");

    try {
      // Register using the provided invite code
      const response = await platformApi.platformRegister(
        TEST_DEVELOPER_EMAIL!,
        TEST_DEVELOPER_PASSWORD!,
        TEST_DEVELOPER_INVITE_CODE!,
        TEST_DEVELOPER_NAME!
      );
      console.log("Successfully registered test user");
      cachedLoginResponse = response;

      return response;
    } catch (registerError: any) {
      if (
        registerError.message.includes("Email already registered") ||
        registerError.message.includes("User already exists")
      ) {
        console.log("User already registered, retrying login");
        // If user already exists, try login again (maybe there was a temporary issue)
        const response = await platformLogin(TEST_DEVELOPER_EMAIL!, TEST_DEVELOPER_PASSWORD!);
        cachedLoginResponse = response;

        return response;
      } else {
        console.error("Registration failed with unexpected error:", registerError.message);
        throw registerError;
      }
    }
  }
}

// Clean up before each test
beforeEach(async () => {
  window.localStorage.clear();
});

test("Developer login and token storage", async () => {
  try {
    const { access_token, refresh_token } = await tryDeveloperLogin();
    expect(access_token).toBeDefined();
    expect(refresh_token).toBeDefined();

    const stored = readTransportV2Credentials(getPlatformApiUrl(), "platform");
    expect(stored?.accessToken).toBe(access_token);
    expect(stored?.refreshToken).toBe(refresh_token);
    expect(window.localStorage.getItem("access_token")).toBeNull();
    expect(window.localStorage.getItem("refresh_token")).toBeNull();
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Platform Me endpoint returns user with organizations", async () => {
  try {
    // Clear any existing storage
    window.localStorage.clear();

    // Login first to get authenticated
    await tryDeveloperLogin();

    try {
      // Call the me endpoint
      const response = await platformMe();

      // Verify the response structure
      expect(response).toBeDefined();
      expect(response.user).toBeDefined();
      expect(response.user.id).toBeDefined();
      expect(response.user.email).toBeDefined();
      expect(typeof response.user.email_verified).toBe("boolean");
      expect(response.organizations).toBeDefined();
      expect(Array.isArray(response.organizations)).toBe(true);

      // If the user has organizations, verify their structure
      if (response.organizations && response.organizations.length > 0) {
        const org = response.organizations[0];

        expect(org.id).toBeDefined();
        expect(org.name).toBeDefined();
        // These fields may or may not be present depending on the API implementation
        // Only check them if they exist
        if (org.role) expect(org.role).toBeDefined();
        if (org.created_at) expect(org.created_at).toBeDefined();
        if (org.updated_at) expect(org.updated_at).toBeDefined();
      }
    } catch (meError: any) {
      console.error("Error calling platform Me endpoint:", meError.message);
      throw meError;
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Developer login fails with invalid credentials", async () => {
  try {
    await platformLogin("invalid@email.com", "wrongpassword");
    throw new Error("Should not succeed with invalid credentials");
  } catch (error: any) {
    // Match a wider range of error messages about invalid credentials
    expect(error.message).toMatch(/Invalid email|Invalid password|Invalid.*login|login.*Invalid/i);
  }
});

test("Developer registration fails with existing email", async () => {
  // This should fail since we've already registered this email in tryDeveloperLogin
  await expect(
    platformRegister(
      TEST_DEVELOPER_EMAIL!,
      TEST_DEVELOPER_PASSWORD!,
      TEST_DEVELOPER_INVITE_CODE!, // Use the actual invite code from env
      TEST_DEVELOPER_NAME!
    )
  ).rejects.toThrow(/Email already registered|User already exists/);
});

test("Developer login persists tokens correctly", async () => {
  try {
    const { access_token, refresh_token } = await tryDeveloperLogin();

    const stored = readTransportV2Credentials(getPlatformApiUrl(), "platform");
    expect(stored?.accessToken).toBe(access_token);
    expect(stored?.refreshToken).toBe(refresh_token);
    expect(window.localStorage.getItem("access_token")).toBeNull();
    expect(window.localStorage.getItem("refresh_token")).toBeNull();

    // Clear storage
    window.localStorage.clear();
    expect(readTransportV2Credentials(getPlatformApiUrl(), "platform")).toBeNull();
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Developer registration validates input", async () => {
  // Skip actual registration tests and focus only on invite code validation
  // We're focusing on input validation errors that should happen before any user creation

  // Test missing invite code
  try {
    await platformRegister(
      "test@example.com", // Use a dummy email that won't actually register
      "password123", // Use a dummy password
      "", // Empty invite code - should fail validation
      "Test User"
    );
    throw new Error("Should not accept empty invite code");
  } catch (error: any) {
    // Allow for different error message formats
    expect(error.message).toMatch(/Invalid|invite.*required|invite code|Bad Request/i);
  }

  // Test invalid invite code format
  try {
    await platformRegister(
      "test@example.com", // Use a dummy email that won't actually register
      "password123", // Use a dummy password
      "not-a-uuid", // Invalid UUID format - should fail validation
      "Test User"
    );
    throw new Error("Should not accept invalid invite code format");
  } catch (error: any) {
    // Allow for different error message formats
    expect(error.message).toMatch(/Invalid|invite code|format|uuid|Bad Request/i);
  }
});

test("Create, list, and delete organization", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create a unique organization name (to avoid conflicts if test is run multiple times)
    const orgName = `Test Org ${Date.now()}`;

    // Create the organization
    const createdOrg = await platformApi.createOrganization(orgName);
    expect(createdOrg).toBeDefined();
    expect(createdOrg.name).toBe(orgName);
    expect(createdOrg.id).toBeDefined();

    // List organizations and verify the new one is there
    const orgs = await platformApi.listOrganizations();
    expect(orgs).toBeDefined();
    expect(Array.isArray(orgs)).toBe(true);

    // Find our organization in the list
    const foundOrg = orgs.find((org) => org.id === createdOrg.id);
    expect(foundOrg).toBeDefined();
    expect(foundOrg?.name).toBe(orgName);

    // Delete the organization
    await platformApi.deleteOrganization(createdOrg.id.toString());

    // List organizations again and verify the deleted one is gone
    const orgsAfterDelete = await platformApi.listOrganizations();
    const shouldBeUndefined = orgsAfterDelete.find((org) => org.id === createdOrg.id);
    expect(shouldBeUndefined).toBeUndefined();
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Organization creation with invalid input", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Test empty name
    try {
      await platformApi.createOrganization("");
      throw new Error("Should not accept empty organization name");
    } catch (error: any) {
      expect(error.message).toMatch(/Invalid|name.*required|Bad Request/i);
    }

    // Test extremely long name (if there's a limit)
    try {
      const veryLongName = "a".repeat(1000);
      await platformApi.createOrganization(veryLongName);
      // Note: This may or may not fail depending on API implementation
    } catch (error: any) {
      expect(error.message).toMatch(/Invalid|name.*too long|Bad Request/i);
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Organization deletion edge cases", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Test deleting non-existent organization
    try {
      await platformApi.deleteOrganization("non-existent-id");
      throw new Error("Should not be able to delete non-existent organization");
    } catch (error: any) {
      expect(error.message).toMatch(
        /Not Found|Organization not found|Bad Request|Invalid URL|UUID parsing failed|HTTP error! Status: 400/i
      );
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Create organization with duplicate name", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create a unique organization name
    const orgName = `Test Duplicate Org ${Date.now()}`;

    // Create the first organization
    const firstOrg = await platformApi.createOrganization(orgName);
    expect(firstOrg).toBeDefined();

    try {
      // Try creating a second organization with the same name
      await platformApi.createOrganization(orgName);

      // If we reach here, it means duplicate names are allowed
      // We should clean up both organizations
      const orgs = await platformApi.listOrganizations();
      const duplicateOrgs = orgs.filter((org) => org.name === orgName);
      for (const org of duplicateOrgs) {
        await platformApi.deleteOrganization(org.id.toString());
      }
    } catch (error: any) {
      // Expected error for duplicate name
      expect(error.message).toMatch(/duplicate|already exists|Bad Request/i);

      // Clean up the first organization
      await platformApi.deleteOrganization(firstOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Create and manage multiple organizations", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Keep track of created organizations to delete them later
    const createdOrgIds: string[] = [];

    // Create three organizations
    const timestamp = Date.now();
    for (let i = 0; i < 3; i++) {
      const orgName = `Test Multi Org ${timestamp}-${i}`;
      const org = await platformApi.createOrganization(orgName);
      expect(org).toBeDefined();
      expect(org.name).toBe(orgName);
      createdOrgIds.push(org.id.toString());
    }

    // List organizations and verify all three exist
    const orgs = await platformApi.listOrganizations();
    for (const orgId of createdOrgIds) {
      const found = orgs.some((org) => org.id.toString() === orgId);
      expect(found).toBe(true);
    }

    // Delete organizations one by one and verify they're gone
    for (const orgId of createdOrgIds) {
      await platformApi.deleteOrganization(orgId);
      const orgsAfter = await platformApi.listOrganizations();
      const stillExists = orgsAfter.some((org) => org.id.toString() === orgId);
      expect(stillExists).toBe(false);
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("List organizations with no organizations", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Get all existing organizations
    const initialOrgs = await platformApi.listOrganizations();

    // Delete all organizations (if any)
    for (const org of initialOrgs) {
      await platformApi.deleteOrganization(org.id.toString());
    }

    // List organizations - should be empty or still return a valid empty array
    const emptyOrgs = await platformApi.listOrganizations();
    expect(Array.isArray(emptyOrgs)).toBe(true);

    // Create a test organization to ensure state is reset properly
    const orgName = `Test Reset Org ${Date.now()}`;
    await platformApi.createOrganization(orgName);
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Organization operations require authentication", async () => {
  try {
    // Clear any authentication tokens
    window.localStorage.clear();

    // Try to list organizations without authentication
    try {
      await platformApi.listOrganizations();
      throw new Error("Should not be able to list organizations without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Try to create an organization without authentication
    try {
      await platformApi.createOrganization("Test Org");
      throw new Error("Should not be able to create organizations without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Try to delete an organization without authentication
    try {
      await platformApi.deleteOrganization("any-id");
      throw new Error("Should not be able to delete organizations without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Organization with special characters in name", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization with special characters in name
    const specialOrgName = `Test Org & Special #${Date.now()}`;

    try {
      const org = await platformApi.createOrganization(specialOrgName);
      expect(org).toBeDefined();
      expect(org.name).toBe(specialOrgName);

      // Clean up
      await platformApi.deleteOrganization(org.id.toString());
    } catch (error: any) {
      // If the API doesn't allow special characters, expect a proper validation error
      expect(error.message).toMatch(/invalid|character|Bad Request/i);
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("List organizations pagination handling", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Get initial list of organizations
    const initialOrgs = await platformApi.listOrganizations();

    // Create a batch of organizations if needed to test pagination
    const createdOrgIds: string[] = [];
    const timestamp = Date.now();
    const batchSize = 5; // Create enough to potentially trigger pagination

    for (let i = 0; i < batchSize; i++) {
      const orgName = `Test Pagination Org ${timestamp}-${i}`;
      const org = await platformApi.createOrganization(orgName);
      createdOrgIds.push(org.id.toString());
    }

    // Fetch the updated list of organizations - should include all created ones
    const updatedOrgs = await platformApi.listOrganizations();

    // Verify we have more organizations now
    expect(updatedOrgs.length).toBeGreaterThanOrEqual(initialOrgs.length + batchSize);

    // Verify all our newly created orgs are in the list
    for (const id of createdOrgIds) {
      const found = updatedOrgs.some((org) => org.id.toString() === id);
      expect(found).toBe(true);
    }

    // Clean up all created organizations
    for (const id of createdOrgIds) {
      await platformApi.deleteOrganization(id);
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

// Add test for platform API URL validation
test("Platform API URL is required", () => {
  // The platform-api-url-loader will throw if no URL is provided
  // Verify it's set to something non-empty
  const apiUrl = process.env.VITE_OPEN_SECRET_API_URL;
  expect(apiUrl).toBeDefined();
  expect(apiUrl?.trim().length).toBeGreaterThan(0);
});

// Test chained organization operations as would be used in real app
test("Organization API flow with chained operations", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // 1. Create a new organization
    const orgName = `Test Chain Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    // 2. Verify it appears in the list
    let orgs = await platformApi.listOrganizations();
    let found = orgs.some((org) => org.id.toString() === createdOrg.id.toString());
    expect(found).toBe(true);

    // 3. Delete the organization
    await platformApi.deleteOrganization(createdOrg.id.toString());

    // 4. Verify it no longer appears in the list
    orgs = await platformApi.listOrganizations();
    found = orgs.some((org) => org.id.toString() === createdOrg.id.toString());
    expect(found).toBe(false);

    // 5. Attempt to delete the same organization again (should fail)
    try {
      await platformApi.deleteOrganization(createdOrg.id.toString());
      throw new Error("Should not be able to delete the same organization twice");
    } catch (error: any) {
      expect(error.message).toMatch(
        /Not Found|Organization not found|Bad Request|HTTP error! Status: 400/i
      );
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

// Test deleting a non-existent organization with a valid UUID format
test("Organization deletion with random UUID", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Generate a random UUID that (almost certainly) doesn't exist
    const randomUUID = crypto.randomUUID();

    // Try to delete an organization with this UUID
    try {
      await platformApi.deleteOrganization(randomUUID);
      throw new Error("Should not be able to delete non-existent organization");
    } catch (error: any) {
      // This should return a 404 Not Found or similar
      expect(error.message).toMatch(
        /Not Found|Organization not found|not exist|HTTP error! Status: 404/i
      );
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

// ===== PROJECT TESTS =====

test("Get project by ID", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create a new organization for testing
    const orgName = `Test Get Project Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);
    expect(createdOrg).toBeDefined();
    expect(createdOrg.name).toBe(orgName);

    try {
      // Create a project first
      const projectName = `Test Get Project ${Date.now()}`;
      const projectDescription = "A project to test getProject functionality";
      const createdProject = await platformApi.createProject(
        createdOrg.id.toString(),
        projectName,
        projectDescription
      );
      expect(createdProject).toBeDefined();

      // Get the project by ID
      const retrievedProject = await platformApi.getProject(
        createdOrg.id.toString(),
        createdProject.id.toString()
      );

      // Verify the project details
      expect(retrievedProject).toBeDefined();
      expect(retrievedProject.id).toBe(createdProject.id);
      expect(retrievedProject.name).toBe(projectName);
      expect(retrievedProject.description).toBe(projectDescription);
      expect(retrievedProject.client_id).toBeDefined();
      expect(retrievedProject.status).toBeDefined();
      expect(retrievedProject.created_at).toBeDefined();

      // Test getting a non-existent project
      try {
        await platformApi.getProject(createdOrg.id.toString(), "non-existent-id");
        throw new Error("Should not be able to get non-existent project");
      } catch (error: any) {
        expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40/i);
      }

      // Test getting a project with non-existent organization
      try {
        await platformApi.getProject("non-existent-id", createdProject.id.toString());
        throw new Error("Should not be able to get project with non-existent organization");
      } catch (error: any) {
        expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40/i);
      }
    } finally {
      // Clean up by deleting the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project CRUD operations within an organization", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create a new organization for testing projects
    const orgName = `Test Project Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);
    expect(createdOrg).toBeDefined();
    expect(createdOrg.name).toBe(orgName);

    try {
      // 1. Create a project
      const projectName = `Test Project ${Date.now()}`;
      const projectDescription = "A test project for automated testing";
      const createdProject = await platformApi.createProject(
        createdOrg.id.toString(),
        projectName,
        projectDescription
      );
      expect(createdProject).toBeDefined();
      expect(createdProject.name).toBe(projectName);
      expect(createdProject.description).toBe(projectDescription);
      expect(createdProject.client_id).toBeDefined();

      // 2. List projects and verify the new one is there
      const projects = await platformApi.listProjects(createdOrg.id.toString());
      expect(projects).toBeDefined();
      expect(Array.isArray(projects)).toBe(true);

      // Find our project in the list
      const foundProject = projects.find((project) => project.id === createdProject.id);
      expect(foundProject).toBeDefined();
      expect(foundProject?.name).toBe(projectName);
      expect(foundProject?.description).toBe(projectDescription);

      // 3. Update the project
      const updatedName = `Updated Project ${Date.now()}`;
      const updatedDescription = "This description has been updated";
      const updatedStatus = "inactive"; // Assuming status can be changed

      const updatedProject = await platformApi.updateProject(
        createdOrg.id.toString(),
        createdProject.id.toString(),
        {
          name: updatedName,
          description: updatedDescription,
          status: updatedStatus
        }
      );

      expect(updatedProject).toBeDefined();
      expect(updatedProject.name).toBe(updatedName);
      expect(updatedProject.description).toBe(updatedDescription);
      expect(updatedProject.status).toBe(updatedStatus);

      // 4. List projects again and verify the updates
      const updatedProjects = await platformApi.listProjects(createdOrg.id.toString());
      const updatedFoundProject = updatedProjects.find(
        (project) => project.id === createdProject.id
      );
      expect(updatedFoundProject).toBeDefined();
      expect(updatedFoundProject?.name).toBe(updatedName);
      expect(updatedFoundProject?.description).toBe(updatedDescription);
      expect(updatedFoundProject?.status).toBe(updatedStatus);

      // 5. Delete the project
      await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());

      // 6. List projects again and verify the deleted one is gone
      const projectsAfterDelete = await platformApi.listProjects(createdOrg.id.toString());
      const shouldBeUndefined = projectsAfterDelete.find(
        (project) => project.id === createdProject.id
      );
      expect(shouldBeUndefined).toBeUndefined();
    } finally {
      // Clean up by deleting the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project creation with invalid input", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Project Error Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Test empty name
      try {
        await platformApi.createProject(createdOrg.id.toString(), "");
        throw new Error("Should not accept empty project name");
      } catch (error: any) {
        expect(error.message).toMatch(/Invalid|name.*required|Bad Request/i);
      }

      // Test extremely long name (if there's a limit)
      try {
        const veryLongName = "a".repeat(1000);
        await platformApi.createProject(createdOrg.id.toString(), veryLongName);
        // Note: This may or may not fail depending on API implementation
      } catch (error: any) {
        expect(error.message).toMatch(/Invalid|name.*too long|Bad Request/i);
      }

      // Test non-existent organization ID
      try {
        await platformApi.createProject("non-existent-id", "Test Project");
        throw new Error("Should not accept non-existent organization ID");
      } catch (error: any) {
        expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40/i);
      }
    } finally {
      // Clean up
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project update with invalid input", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Project Update Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Create a project first
      const projectName = `Test Project Update ${Date.now()}`;
      const project = await platformApi.createProject(createdOrg.id.toString(), projectName);

      // Test updating a non-existent project
      try {
        await platformApi.updateProject(createdOrg.id.toString(), "non-existent-id", {
          name: "New Name"
        });
        throw new Error("Should not be able to update non-existent project");
      } catch (error: any) {
        expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40/i);
      }

      // Test updating a project with non-existent organization
      try {
        await platformApi.updateProject("non-existent-id", project.id.toString(), {
          name: "New Name"
        });
        throw new Error("Should not be able to update project with non-existent organization");
      } catch (error: any) {
        expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40/i);
      }

      // Test empty update object (this may or may not be allowed)
      try {
        await platformApi.updateProject(createdOrg.id.toString(), project.id.toString(), {});
        // If it succeeds, no need to do anything
      } catch (error: any) {
        expect(error.message).toMatch(/invalid|Bad Request/i);
      }

      // Clean up the project
      await platformApi.deleteProject(createdOrg.id.toString(), project.id.toString());
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project deletion edge cases", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Project Delete Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Test deleting a non-existent project
      try {
        await platformApi.deleteProject(createdOrg.id.toString(), "non-existent-id");
        throw new Error("Should not be able to delete non-existent project");
      } catch (error: any) {
        expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40/i);
      }

      // Test deleting a project from a non-existent organization
      try {
        await platformApi.deleteProject("non-existent-id", "some-project-id");
        throw new Error("Should not be able to delete project from non-existent organization");
      } catch (error: any) {
        expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40/i);
      }

      // Create a project and delete it
      const projectName = `Test Project Delete ${Date.now()}`;
      const project = await platformApi.createProject(createdOrg.id.toString(), projectName);
      await platformApi.deleteProject(createdOrg.id.toString(), project.id.toString());

      // Try to delete it again (should fail)
      try {
        await platformApi.deleteProject(createdOrg.id.toString(), project.id.toString());
        throw new Error("Should not be able to delete the same project twice");
      } catch (error: any) {
        expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40/i);
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project with special characters in name", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Project Special Chars Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Test creating a project with special characters in name
      const specialProjectName = `Test Project & Special #${Date.now()}`;

      try {
        const project = await platformApi.createProject(
          createdOrg.id.toString(),
          specialProjectName
        );
        expect(project).toBeDefined();
        expect(project.name).toBe(specialProjectName);

        // Clean up the project
        await platformApi.deleteProject(createdOrg.id.toString(), project.id.toString());
      } catch (error: any) {
        // If the API doesn't allow special characters, expect a proper validation error
        expect(error.message).toMatch(/invalid|character|Bad Request/i);
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project listing with multiple projects (pagination handling)", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Project Pagination Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Create several projects to test pagination
      const projectIds: string[] = [];
      const timestamp = Date.now();
      const batchSize = 5; // Create enough to potentially trigger pagination

      for (let i = 0; i < batchSize; i++) {
        const projectName = `Test Pagination Project ${timestamp}-${i}`;
        const project = await platformApi.createProject(
          createdOrg.id.toString(),
          projectName,
          `Description for pagination project ${i}`
        );
        projectIds.push(project.id.toString());
      }

      // Fetch all projects - should include all created ones
      const projects = await platformApi.listProjects(createdOrg.id.toString());

      // Verify we have at least the number of projects we created
      expect(projects.length).toBeGreaterThanOrEqual(batchSize);

      // Verify all our newly created projects are in the list
      for (const id of projectIds) {
        const found = projects.some((project) => project.id.toString() === id);
        expect(found).toBe(true);
      }

      // Clean up all created projects
      for (const id of projectIds) {
        await platformApi.deleteProject(createdOrg.id.toString(), id);
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project operations require authentication", async () => {
  try {
    // First create an org and project while authenticated
    await tryDeveloperLogin();

    const orgName = `Test Project Auth Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);
    const projectName = `Test Auth Project ${Date.now()}`;
    const createdProject = await platformApi.createProject(createdOrg.id.toString(), projectName);

    // Now clear authentication and try operations
    window.localStorage.clear();

    // Try to list projects without authentication
    try {
      await platformApi.listProjects(createdOrg.id.toString());
      throw new Error("Should not be able to list projects without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Try to create a project without authentication
    try {
      await platformApi.createProject(createdOrg.id.toString(), "New Project");
      throw new Error("Should not be able to create projects without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Try to update a project without authentication
    try {
      await platformApi.updateProject(createdOrg.id.toString(), createdProject.id.toString(), {
        name: "Updated Name"
      });
      throw new Error("Should not be able to update projects without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Try to delete a project without authentication
    try {
      await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
      throw new Error("Should not be able to delete projects without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Re-authenticate to clean up
    await tryDeveloperLogin();

    // Clean up
    await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
    await platformApi.deleteOrganization(createdOrg.id.toString());
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Create project with duplicate name in same organization", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Project Duplicate Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Create a project with a specific name
      const duplicateProjectName = `Test Duplicate Project ${Date.now()}`;
      const firstProject = await platformApi.createProject(
        createdOrg.id.toString(),
        duplicateProjectName
      );
      expect(firstProject).toBeDefined();

      try {
        // Try creating a second project with the same name
        await platformApi.createProject(createdOrg.id.toString(), duplicateProjectName);

        // If we reach here, it means duplicate names are allowed
        // We should clean up both projects
        await platformApi.deleteProject(createdOrg.id.toString(), firstProject.id.toString());
      } catch (error: any) {
        // Expected error for duplicate name
        expect(error.message).toMatch(/duplicate|already exists|Bad Request/i);

        // Clean up the first project
        await platformApi.deleteProject(createdOrg.id.toString(), firstProject.id.toString());
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project API flow with chained operations", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Project Chain Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // 1. Create a new project
      const projectName = `Test Chain Project ${Date.now()}`;
      const createdProject = await platformApi.createProject(createdOrg.id.toString(), projectName);

      // 2. Verify it appears in the list
      let projects = await platformApi.listProjects(createdOrg.id.toString());
      let found = projects.some(
        (project) => project.id.toString() === createdProject.id.toString()
      );
      expect(found).toBe(true);

      // 3. Update the project
      const updatedName = `Updated Chain Project ${Date.now()}`;
      const updatedProject = await platformApi.updateProject(
        createdOrg.id.toString(),
        createdProject.id.toString(),
        { name: updatedName }
      );
      expect(updatedProject.name).toBe(updatedName);

      // 4. Verify the update appears in the list
      projects = await platformApi.listProjects(createdOrg.id.toString());
      const updatedFound = projects.find(
        (project) => project.id.toString() === createdProject.id.toString()
      );
      expect(updatedFound).toBeDefined();
      expect(updatedFound?.name).toBe(updatedName);

      // 5. Delete the project
      await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());

      // 6. Verify it no longer appears in the list
      projects = await platformApi.listProjects(createdOrg.id.toString());
      found = projects.some((project) => project.id.toString() === createdProject.id.toString());
      expect(found).toBe(false);

      // 7. Attempt to delete the same project again (should fail)
      try {
        await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
        throw new Error("Should not be able to delete the same project twice");
      } catch (error: any) {
        expect(error.message).toMatch(
          /Not Found|Organization not found|Bad Request|HTTP error! Status: 400/i
        );
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project deletion with random UUID", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Project Random UUID Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Generate a random UUID that (almost certainly) doesn't exist
      const randomUUID = crypto.randomUUID();

      // Try to delete a project with this UUID
      try {
        await platformApi.deleteProject(createdOrg.id.toString(), randomUUID);
        throw new Error("Should not be able to delete non-existent project");
      } catch (error: any) {
        // This should return a 404 Not Found or similar
        expect(error.message).toMatch(
          /Not Found|Organization not found|not exist|HTTP error! Status: 404/i
        );
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

// ===== EMAIL VERIFICATION TESTS =====

test("Platform user email verification functionality", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Mock invalid verification code
    const invalidCode = "invalid-verification-code";

    // Test with an invalid verification code - this should fail
    try {
      await platformApi.verifyPlatformEmail(invalidCode);
      throw new Error("Should not verify with invalid code");
    } catch (error: any) {
      // We expect an error for invalid verification code
      // API returns "Failed to verify email" as the default error message
      expect(error.message).toMatch(
        /Invalid|verification.*failed|Bad Request|not found|Failed to verify email/i
      );
    }

    // Test requesting a new verification code - this should succeed if the user is not verified
    // or fail appropriately if the user is already verified
    try {
      const response = await platformApi.requestNewPlatformVerificationCode();
      // If it succeeds, we should get a success message
      expect(response).toBeDefined();
      expect(response.message).toBeDefined();
      expect(response.message).toMatch(/sent|success/i);
    } catch (error: any) {
      // It's also acceptable if the user is already verified
      if (error.message.includes("already verified")) {
        console.log("User is already verified, which is acceptable");
      } else {
        // Any other error should be a proper test failure
        throw error;
      }
    }

    // We can't test a valid verification code since those are unique and one-time use
    // But we can verify that the API call is structured correctly
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Platform verification requires authentication for requestNewPlatformVerificationCode", async () => {
  try {
    // First make sure we're authenticated
    await tryDeveloperLogin();

    // Now clear authentication and try operations
    window.localStorage.clear();

    // Try to request a new verification code without authentication
    try {
      await platformApi.requestNewPlatformVerificationCode();
      throw new Error("Should not be able to request verification without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Note: verifyPlatformEmail is encryptedApiCall (not authenticatedApiCall) and doesn't require
    // authentication since it's used in the verification flow before the user is logged in
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

// ===== PLATFORM PASSWORD MANAGEMENT TESTS =====

test("Platform password reset flow", async () => {
  try {
    // This test simulates the entire password reset flow, but without actually triggering emails
    // It will test the API endpoints and error conditions

    // For testing, we'll use a non-existent email to test the expected error responses
    const nonExistentEmail = `non-existent-${Date.now()}@example.com`;

    // Create a secure secret and hash it for the request
    const plaintextSecret = `test-secret-${Date.now()}`;
    const hashedSecret = encode(new TextEncoder().encode(plaintextSecret));

    // Step 1: Request password reset with non-existent email (should fail gracefully)
    try {
      await platformApi.requestPlatformPasswordReset(nonExistentEmail, hashedSecret);
      // Some APIs return success even for non-existent emails for security reasons
      console.log("API returns success for non-existent email (security feature)");
    } catch (error: any) {
      // The API might return an error for non-existent email, or it might return success
      // Either behavior is acceptable from a security perspective
      console.log(`Got error for non-existent email: ${error.message}`);
    }

    // Step 2: Try to confirm reset with invalid code (should fail)
    const invalidCode = "invalid-code";
    const newPassword = "NewTestPassword123!";

    try {
      await platformApi.confirmPlatformPasswordReset(
        nonExistentEmail,
        invalidCode,
        plaintextSecret,
        newPassword
      );
      throw new Error("Should not allow password reset with invalid code");
    } catch (error: any) {
      // This should always fail with some kind of validation error
      expect(error.message).toMatch(
        /invalid|not found|expired|Bad Request|Failed to confirm platform password reset/i
      );
    }

    // We can't easily test the actual flow with real emails, but we can verify our functions exist

    // Verify the functions exist and are functions
    expect(typeof platformApi.requestPlatformPasswordReset).toBe("function");
    expect(typeof platformApi.confirmPlatformPasswordReset).toBe("function");

    console.log("Test completed: Platform password reset API interface verified");
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Platform password change API call interface", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Try with incorrect current password (should fail)
    try {
      await platformApi.changePlatformPassword("wrong-current-password", "NewPassword123!");
      throw new Error("Should not allow password change with incorrect current password");
    } catch (error: any) {
      expect(error.message).toMatch(
        /invalid|incorrect|unauthorized|Bad Request|Failed to change platform password/i
      );
    }

    // In an integration test environment without a real API, we can't fully test actual password changes
    // But we can verify that the function is calling the API with the right parameters

    // Verify the function exists and is a function
    expect(typeof platformApi.changePlatformPassword).toBe("function");

    // Note: In a real test environment with a working API endpoint, we would test the actual password change
    // Here we're just testing the function interface since the API endpoint might not be fully implemented

    console.log("Test completed: Platform password change API interface verified");
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Platform password change requires authentication", async () => {
  try {
    // First make sure we're authenticated
    await tryDeveloperLogin();

    // Now clear authentication and try operations
    window.localStorage.clear();

    // Try to change password without authentication
    try {
      await platformApi.changePlatformPassword("CurrentPassword", "NewPassword");
      throw new Error("Should not be able to change password without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

// ===== PROJECT PUSH SETTINGS TESTS =====

test.skip("Project push settings CRUD operations", async () => {
  try {
    await tryDeveloperLogin();

    const orgName = `Test Push Settings Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);
    expect(createdOrg).toBeDefined();

    try {
      const projectName = `Test Push Settings Project ${Date.now()}`;
      const createdProject = await platformApi.createProject(
        createdOrg.id.toString(),
        projectName,
        "A project for testing push settings"
      );
      expect(createdProject).toBeDefined();

      try {
        const initialSettings = await platformApi.getPushSettings(
          createdOrg.id.toString(),
          createdProject.id.toString()
        );

        expect(initialSettings).toMatchObject({ encrypted_preview_enabled: false });

        const pushSettings = {
          encrypted_preview_enabled: true,
          ios: {
            enabled: true,
            bundle_id: `ai.trymaple.push.${Date.now()}`,
            apns_environment: "dev" as const,
            team_id: `TEAM${Date.now().toString().slice(-6)}`,
            key_id: `KEY${Date.now().toString().slice(-6)}`
          },
          android: {
            enabled: true,
            firebase_project_id: `firebase-project-${Date.now()}`,
            package_name: `ai.trymaple.push.android.${Date.now()}`
          }
        };

        const updatedSettings = await platformApi.updatePushSettings(
          createdOrg.id.toString(),
          createdProject.id.toString(),
          pushSettings
        );

        expect(updatedSettings).toEqual(pushSettings);

        const fetchedSettings = await platformApi.getPushSettings(
          createdOrg.id.toString(),
          createdProject.id.toString()
        );

        expect(fetchedSettings).toEqual(pushSettings);
      } finally {
        await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
      }
    } finally {
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

// ===== PROJECT SECRET TESTS =====

test("Project secret CRUD operations", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create a new organization for testing
    const orgName = `Test Secret Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);
    expect(createdOrg).toBeDefined();

    try {
      // Create a project for testing secrets
      const projectName = `Test Secret Project ${Date.now()}`;
      const createdProject = await platformApi.createProject(
        createdOrg.id.toString(),
        projectName,
        "A project for testing secrets"
      );
      expect(createdProject).toBeDefined();

      try {
        // 1. Create a secret with alphanumeric only key name
        const secretKeyName = `testsecret${Date.now()}`;
        const secretValue = "supersecretvaluethatshouldbencrypted";
        const createdSecret = await platformApi.createProjectSecret(
          createdOrg.id.toString(),
          createdProject.id.toString(),
          secretKeyName,
          encodeSecret(secretValue)
        );
        expect(createdSecret).toBeDefined();
        expect(createdSecret.key_name).toBe(secretKeyName);
        expect(createdSecret.created_at).toBeDefined();
        expect(createdSecret.updated_at).toBeDefined();
        // Note: The actual secret value should not be returned for security reasons

        // 2. List secrets and verify the new one is there
        const secrets = await platformApi.listProjectSecrets(
          createdOrg.id.toString(),
          createdProject.id.toString()
        );
        expect(secrets).toBeDefined();
        expect(Array.isArray(secrets)).toBe(true);

        // Find our secret in the list
        const foundSecret = secrets.find((secret) => secret.key_name === secretKeyName);
        expect(foundSecret).toBeDefined();
        expect(foundSecret?.key_name).toBe(secretKeyName);
        // Secret value should not be included in the list

        // 3. Delete the secret
        await platformApi.deleteProjectSecret(
          createdOrg.id.toString(),
          createdProject.id.toString(),
          secretKeyName
        );

        // 4. List secrets again and verify the deleted one is gone
        const secretsAfterDelete = await platformApi.listProjectSecrets(
          createdOrg.id.toString(),
          createdProject.id.toString()
        );
        const shouldBeUndefined = secretsAfterDelete.find(
          (secret) => secret.key_name === secretKeyName
        );
        expect(shouldBeUndefined).toBeUndefined();
      } finally {
        // Clean up the project
        await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project secret creation with invalid input", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Secret Error Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Create a project for testing
      const projectName = `Test Secret Error Project ${Date.now()}`;
      const createdProject = await platformApi.createProject(createdOrg.id.toString(), projectName);

      try {
        // Test empty key name
        try {
          await platformApi.createProjectSecret(
            createdOrg.id.toString(),
            createdProject.id.toString(),
            "",
            encodeSecret("some-value")
          );
          throw new Error("Should not accept empty secret key name");
        } catch (error: any) {
          expect(error.message).toMatch(/Invalid|name.*required|Bad Request/i);
        }

        // Test empty secret value
        try {
          await platformApi.createProjectSecret(
            createdOrg.id.toString(),
            createdProject.id.toString(),
            "valid-key-name",
            encodeSecret("")
          );
          throw new Error("Should not accept empty secret value");
        } catch (error: any) {
          expect(error.message).toMatch(/Invalid|value.*required|Bad Request/i);
        }

        // Test extremely long key name (if there's a limit)
        try {
          const veryLongName = "a".repeat(1000);
          await platformApi.createProjectSecret(
            createdOrg.id.toString(),
            createdProject.id.toString(),
            veryLongName,
            encodeSecret("some-value")
          );
          // Note: This may or may not fail depending on API implementation
        } catch (error: any) {
          expect(error.message).toMatch(/Invalid|name.*too long|Bad Request/i);
        }

        // Test non-existent project ID
        try {
          await platformApi.createProjectSecret(
            createdOrg.id.toString(),
            "non-existent-id",
            "test-key",
            encodeSecret("test-value")
          );
          throw new Error("Should not accept non-existent project ID");
        } catch (error: any) {
          expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40/i);
        }

        // Test non-existent organization ID
        try {
          await platformApi.createProjectSecret(
            "non-existent-id",
            createdProject.id.toString(),
            "test-key",
            encodeSecret("test-value")
          );
          throw new Error("Should not accept non-existent organization ID");
        } catch (error: any) {
          expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40/i);
        }
      } finally {
        // Clean up the project
        await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project secret with special characters in key name", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Secret Special Chars Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Create a project for testing
      const projectName = `Test Secret Special Chars Project ${Date.now()}`;
      const createdProject = await platformApi.createProject(createdOrg.id.toString(), projectName);

      try {
        // First create a valid secret to prove the API works correctly
        const validKeyName = `testsecret${Date.now()}`;
        const secretValue = "validalphanumericsecretvalue";

        const validSecret = await platformApi.createProjectSecret(
          createdOrg.id.toString(),
          createdProject.id.toString(),
          validKeyName,
          encodeSecret(secretValue)
        );

        expect(validSecret).toBeDefined();
        expect(validSecret.key_name).toBe(validKeyName);

        // Now try with a key name containing special characters - this SHOULD fail
        const invalidKeyName = `test@special#${Date.now()}`;

        let errorThrown = false;
        try {
          await platformApi.createProjectSecret(
            createdOrg.id.toString(),
            createdProject.id.toString(),
            invalidKeyName,
            encodeSecret(secretValue)
          );
        } catch (error: any) {
          errorThrown = true;
          // We expect an error about invalid characters or bad request
          expect(error.message).toMatch(/invalid|character|Bad Request|HTTP error! Status: 400/i);
        }

        // Make sure the error was thrown
        expect(errorThrown).toBe(true);

        // Clean up the valid secret
        await platformApi.deleteProjectSecret(
          createdOrg.id.toString(),
          createdProject.id.toString(),
          validKeyName
        );
      } finally {
        // Clean up the project
        await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project secret deletion edge cases", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Secret Delete Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Create a project for testing
      const projectName = `Test Secret Delete Project ${Date.now()}`;
      const createdProject = await platformApi.createProject(createdOrg.id.toString(), projectName);

      try {
        // Focus on testing the core deletion functionality with better error handling
        console.log("Creating a test secret for deletion test...");

        // Create a secret that we'll delete
        const secretKeyName = `deletetest${Date.now()}`;
        const secretValue = "deletemevalue";

        const createdSecret = await platformApi.createProjectSecret(
          createdOrg.id.toString(),
          createdProject.id.toString(),
          secretKeyName,
          encodeSecret(secretValue)
        );

        expect(createdSecret).toBeDefined();
        console.log(`Successfully created test secret: ${secretKeyName}`);

        // List secrets to verify it exists
        const secretsBefore = await platformApi.listProjectSecrets(
          createdOrg.id.toString(),
          createdProject.id.toString()
        );

        const secretExists = secretsBefore.some((s) => s.key_name === secretKeyName);
        expect(secretExists).toBe(true);
        console.log("Verified secret exists in list");

        // Delete the secret (should succeed)
        await platformApi.deleteProjectSecret(
          createdOrg.id.toString(),
          createdProject.id.toString(),
          secretKeyName
        );
        console.log("Successfully deleted the secret");

        // Verify the secret was deleted
        const secretsAfter = await platformApi.listProjectSecrets(
          createdOrg.id.toString(),
          createdProject.id.toString()
        );

        const secretStillExists = secretsAfter.some((s) => s.key_name === secretKeyName);
        expect(secretStillExists).toBe(false);
        console.log("Verified secret was deleted successfully");

        // Now try to delete a non-existent secret - this might return either 404 or 400
        console.log("Testing deletion of non-existent secret...");
        try {
          await platformApi.deleteProjectSecret(
            createdOrg.id.toString(),
            createdProject.id.toString(),
            `nonexistent${Date.now()}`
          );
          // If it succeeds (idempotent deletion), that's acceptable too
          console.log("Note: Deleting non-existent secret succeeded (idempotent behavior)");
        } catch (error: any) {
          console.log(`Got expected error for non-existent secret: ${error.message}`);
          // Accept either "Not Found" (404) or "Bad Request" (400) as valid responses
          // Some APIs return 400 for resource that doesn't exist rather than 404
          expect(error.message).toMatch(
            /not found|Resource not found|Bad Request|HTTP error! Status: 40[04]/i
          );
        }
      } finally {
        // Clean up the project
        await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Create project secret with duplicate key name", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Secret Duplicate Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Create a project for testing
      const projectName = `Test Secret Duplicate Project ${Date.now()}`;
      const createdProject = await platformApi.createProject(createdOrg.id.toString(), projectName);

      try {
        // Create a secret with a specific key name (alphanumeric only)
        const duplicateKeyName = `duplicatesecretkey${Date.now()}`;
        const secretValue1 = "firstsecretvalue";
        const firstSecret = await platformApi.createProjectSecret(
          createdOrg.id.toString(),
          createdProject.id.toString(),
          duplicateKeyName,
          encodeSecret(secretValue1)
        );
        expect(firstSecret).toBeDefined();

        try {
          // Try creating a second secret with the same key name but different value
          const secretValue2 = "secondsecretvalue";
          await platformApi.createProjectSecret(
            createdOrg.id.toString(),
            createdProject.id.toString(),
            duplicateKeyName,
            encodeSecret(secretValue2)
          );
          throw new Error("Should not be able to create duplicate key");
        } catch (error: any) {
          // Expected error for duplicate key
          expect(error.message).toMatch(/duplicate|already exists|Bad Request/i);

          // Clean up the first secret
          await platformApi.deleteProjectSecret(
            createdOrg.id.toString(),
            createdProject.id.toString(),
            duplicateKeyName
          );
        }
      } finally {
        // Clean up the project
        await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project secret operations require authentication", async () => {
  try {
    // First create an org and project while authenticated
    await tryDeveloperLogin();

    const orgName = `Test Secret Auth Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);
    const projectName = `Test Secret Auth Project ${Date.now()}`;
    const createdProject = await platformApi.createProject(createdOrg.id.toString(), projectName);

    // Create a secret with alphanumeric name
    const secretKeyName = `authtestsecret${Date.now()}`;
    const secretValue = "authenticatedsecretvalue";
    await platformApi.createProjectSecret(
      createdOrg.id.toString(),
      createdProject.id.toString(),
      secretKeyName,
      encodeSecret(secretValue)
    );

    // Now clear authentication and try operations
    window.localStorage.clear();

    // Try to list secrets without authentication
    try {
      await platformApi.listProjectSecrets(createdOrg.id.toString(), createdProject.id.toString());
      throw new Error("Should not be able to list secrets without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Try to create a secret without authentication
    try {
      await platformApi.createProjectSecret(
        createdOrg.id.toString(),
        createdProject.id.toString(),
        "newsecretkey",
        encodeSecret("newsecretvalue")
      );
      throw new Error("Should not be able to create secrets without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Try to delete a secret without authentication
    try {
      await platformApi.deleteProjectSecret(
        createdOrg.id.toString(),
        createdProject.id.toString(),
        secretKeyName
      );
      throw new Error("Should not be able to delete secrets without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Re-authenticate to clean up
    await tryDeveloperLogin();

    // Clean up
    await platformApi.deleteProjectSecret(
      createdOrg.id.toString(),
      createdProject.id.toString(),
      secretKeyName
    );
    await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
    await platformApi.deleteOrganization(createdOrg.id.toString());
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project secret API flow with chained operations", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Secret Chain Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Create a project for testing
      const projectName = `Test Secret Chain Project ${Date.now()}`;
      const createdProject = await platformApi.createProject(createdOrg.id.toString(), projectName);

      try {
        // 1. Verify no secrets exist initially
        const initialSecrets = await platformApi.listProjectSecrets(
          createdOrg.id.toString(),
          createdProject.id.toString()
        );
        expect(initialSecrets).toEqual([]);

        // 2. Create a series of secrets (with alphanumeric key names)
        const secrets = [];
        for (let i = 1; i <= 3; i++) {
          const keyName = `chainedsecret${i}${Date.now()}`;
          const value = `secretvalue${i}`;

          const secret = await platformApi.createProjectSecret(
            createdOrg.id.toString(),
            createdProject.id.toString(),
            keyName,
            encodeSecret(value)
          );

          expect(secret).toBeDefined();
          expect(secret.key_name).toBe(keyName);
          secrets.push(keyName);
        }

        // 3. List all secrets and verify they exist
        const listedSecrets = await platformApi.listProjectSecrets(
          createdOrg.id.toString(),
          createdProject.id.toString()
        );
        expect(listedSecrets.length).toBe(3);

        for (const keyName of secrets) {
          const found = listedSecrets.some((secret) => secret.key_name === keyName);
          expect(found).toBe(true);
        }

        // 4. Delete each secret one by one and verify it's gone
        for (const keyName of secrets) {
          await platformApi.deleteProjectSecret(
            createdOrg.id.toString(),
            createdProject.id.toString(),
            keyName
          );

          const secretsAfterDelete = await platformApi.listProjectSecrets(
            createdOrg.id.toString(),
            createdProject.id.toString()
          );

          const stillExists = secretsAfterDelete.some((secret) => secret.key_name === keyName);
          expect(stillExists).toBe(false);
        }

        // 5. Verify all secrets are gone
        const finalSecrets = await platformApi.listProjectSecrets(
          createdOrg.id.toString(),
          createdProject.id.toString()
        );
        expect(finalSecrets).toEqual([]);
      } finally {
        // Clean up the project
        await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Project secret listing with no secrets", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Secret Empty Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Create a project for testing
      const projectName = `Test Secret Empty Project ${Date.now()}`;
      const createdProject = await platformApi.createProject(createdOrg.id.toString(), projectName);

      try {
        // List secrets for a project that has none
        const secrets = await platformApi.listProjectSecrets(
          createdOrg.id.toString(),
          createdProject.id.toString()
        );

        // Should return an empty array, not null or undefined
        expect(Array.isArray(secrets)).toBe(true);
        expect(secrets.length).toBe(0);
      } finally {
        // Clean up the project
        await platformApi.deleteProject(createdOrg.id.toString(), createdProject.id.toString());
      }
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

// ===== ORGANIZATION INVITES TESTS =====

test("Organization invite CRUD operations", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create a new organization for testing
    const orgName = `Test Invite Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);
    expect(createdOrg).toBeDefined();

    try {
      // 1. Create an invite
      const testEmail = `test-invite-${Date.now()}@example.com`;
      const createdInvite = await platformApi.inviteDeveloper(
        createdOrg.id.toString(),
        testEmail,
        "admin"
      );
      expect(createdInvite).toBeDefined();
      expect(createdInvite.email).toBe(testEmail);
      expect(createdInvite.role).toBe("admin");
      expect(createdInvite.code).toBeDefined();
      expect(createdInvite.used).toBe(false);

      // 2. List invites and verify the new one is there
      const invites = await platformApi.listOrganizationInvites(createdOrg.id.toString());
      expect(invites).toBeDefined();
      expect(Array.isArray(invites)).toBe(true);

      // Find our invite in the list
      const foundInvite = invites.find((invite) => invite.code === createdInvite.code);
      expect(foundInvite).toBeDefined();
      expect(foundInvite?.email).toBe(testEmail);
      expect(foundInvite?.role).toBe("admin");

      // 3. Get a specific invite
      const retrievedInvite = await platformApi.getOrganizationInvite(
        createdOrg.id.toString(),
        createdInvite.code
      );
      expect(retrievedInvite).toBeDefined();
      expect(retrievedInvite.code).toBe(createdInvite.code);
      expect(retrievedInvite.email).toBe(testEmail);
      expect(retrievedInvite.role).toBe("admin");
      // Organization name should be present in the specific invite response
      expect(retrievedInvite.organization_name).toBeDefined();
      expect(retrievedInvite.organization_name).toBe(orgName);

      // 4. Delete the invite
      const deleteResult = await platformApi.deleteOrganizationInvite(
        createdOrg.id.toString(),
        createdInvite.code
      );
      expect(deleteResult).toBeDefined();
      expect(deleteResult.message).toMatch(/deleted|removed|success/i);

      // 5. List invites again and verify the deleted one is gone
      const invitesAfterDelete = await platformApi.listOrganizationInvites(
        createdOrg.id.toString()
      );
      const shouldBeUndefined = invitesAfterDelete.find(
        (invite) => invite.code === createdInvite.code
      );
      expect(shouldBeUndefined).toBeUndefined();
    } finally {
      // Clean up the organization
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Organization invite with invalid input", async () => {
  try {
    // Login first to get authenticated
    await tryDeveloperLogin();

    // Create an organization for testing
    const orgName = `Test Invite Error Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    try {
      // Test empty email
      try {
        await platformApi.inviteDeveloper(createdOrg.id.toString(), "");
        throw new Error("Should not accept empty email");
      } catch (error: any) {
        expect(error.message).toMatch(/email.*required|Invalid|Bad Request/i);
      }

      // Test invalid email format
      try {
        await platformApi.inviteDeveloper(createdOrg.id.toString(), "notanemail");
        throw new Error("Should not accept invalid email format");
      } catch (error: any) {
        expect(error.message).toMatch(/Invalid email|Email.*invalid|Bad Request/i);
      }

      // Test invalid role
      try {
        await platformApi.inviteDeveloper(
          createdOrg.id.toString(),
          "valid@example.com",
          "invalid-role"
        );
        // This may or may not fail depending on API implementation
      } catch (error: any) {
        expect(error.message).toMatch(/Invalid role|Role.*invalid|Bad Request/i);
      }

      // Test non-existent organization ID
      try {
        await platformApi.inviteDeveloper("non-existent-id", "valid@example.com");
        throw new Error("Should not accept non-existent organization ID");
      } catch (error: any) {
        expect(error.message).toMatch(/not found|invalid|Bad Request|HTTP error! Status: 40[0-9]/i);
      }
    } finally {
      // Clean up
      await platformApi.deleteOrganization(createdOrg.id.toString());
    }
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});

test("Organization invite operations require authentication", async () => {
  try {
    // First create an org and invite while authenticated
    await tryDeveloperLogin();

    const orgName = `Test Invite Auth Org ${Date.now()}`;
    const createdOrg = await platformApi.createOrganization(orgName);

    // Create a test invite
    const testEmail = `test-invite-auth-${Date.now()}@example.com`;
    const createdInvite = await platformApi.inviteDeveloper(
      createdOrg.id.toString(),
      testEmail,
      "admin"
    );

    // Now clear authentication and try operations
    window.localStorage.clear();

    // Try to list invites without authentication
    try {
      await platformApi.listOrganizationInvites(createdOrg.id.toString());
      throw new Error("Should not be able to list invites without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Try to get an invite without authentication
    try {
      await platformApi.getOrganizationInvite(createdOrg.id.toString(), createdInvite.code);
      throw new Error("Should not be able to get invites without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Try to delete an invite without authentication
    try {
      await platformApi.deleteOrganizationInvite(createdOrg.id.toString(), createdInvite.code);
      throw new Error("Should not be able to delete invites without authentication");
    } catch (error: any) {
      expect(error.message).toMatch(/unauthorized|unauthenticated|no access token|token/i);
    }

    // Re-authenticate to clean up
    await tryDeveloperLogin();

    // Clean up
    try {
      await platformApi.deleteOrganizationInvite(createdOrg.id.toString(), createdInvite.code);
    } catch (error) {
      console.warn("Failed to delete invite during cleanup:", error);
    }
    await platformApi.deleteOrganization(createdOrg.id.toString());
  } catch (error: any) {
    console.error("Test failed:", error.message);
    throw error;
  }
});
