import { expect, test } from "bun:test";
import {
  fetchLogin,
  fetchSignUp,
  fetchGuestLogin,
  fetchGuestSignUp,
  fetchLogout,
  refreshToken,
  fetchUser,
  changePassword,
  generateThirdPartyToken,
  encryptData,
  decryptData,
  requestAccountDeletion,
  createInstruction,
  listInstructions,
  getInstruction,
  updateInstruction,
  deleteInstruction,
  setDefaultInstruction,
  fetchPut,
  fetchList,
  fetchDeleteAllKV
} from "../../api";
import { readTransportV2Credentials } from "../../transportV2/auth";

const TEST_EMAIL = process.env.VITE_TEST_EMAIL;
const TEST_PASSWORD = process.env.VITE_TEST_PASSWORD;
const TEST_NAME = process.env.VITE_TEST_NAME;
const TEST_CLIENT_ID = process.env.VITE_TEST_CLIENT_ID;
const API_URL = process.env.VITE_OPEN_SECRET_API_URL;

if (!TEST_EMAIL || !TEST_PASSWORD || !TEST_NAME || !TEST_CLIENT_ID || !API_URL) {
  throw new Error("Test credentials must be set in .env.local");
}

async function tryEmailLogin() {
  // Ensure the test user exists
  try {
    return await fetchLogin(TEST_EMAIL!, TEST_PASSWORD!, TEST_CLIENT_ID!);
  } catch (error) {
    console.warn(error);
    console.log("Login failed, attempting signup");
    await fetchSignUp(TEST_EMAIL!, TEST_PASSWORD!, "", TEST_CLIENT_ID!, TEST_NAME!);
    return await fetchLogin(TEST_EMAIL!, TEST_PASSWORD!, TEST_CLIENT_ID!);
  }
}

test("Login with email and fetch user data", async () => {
  const { access_token, refresh_token } = await tryEmailLogin();
  expect(access_token).toBeDefined();
  expect(refresh_token).toBeDefined();
  window.localStorage.setItem("access_token", access_token);
  window.localStorage.setItem("refresh_token", refresh_token);

  const userResponse = await fetchUser();
  expect(userResponse.user.email).toBe(TEST_EMAIL);
});

test("Refresh token works", async () => {
  await tryEmailLogin();

  const refreshResponse = await refreshToken();
  expect(refreshResponse.access_token).toBeDefined();
  expect(refreshResponse.refresh_token).toBeDefined();

  // Verify the new access token works
  const userResponse = await fetchUser();
  expect(userResponse.user.email).toBe(TEST_EMAIL);
});

test("Logout doesn't error", async () => {
  const { refresh_token } = await tryEmailLogin();
  await fetchLogout(refresh_token);
});

test("Guest signup and login flow", async () => {
  // Sign up as guest
  const guestSignup = await fetchGuestSignUp(TEST_PASSWORD!, "", TEST_CLIENT_ID!);
  expect(guestSignup.id).toBeDefined();
  expect(guestSignup.email).toBeNull();
  expect(guestSignup.access_token).toBeDefined();
  expect(guestSignup.refresh_token).toBeDefined();

  // Login as guest
  const guestLogin = await fetchGuestLogin(guestSignup.id, TEST_PASSWORD!, TEST_CLIENT_ID!);
  expect(guestLogin.id).toBe(guestSignup.id);
  expect(guestLogin.access_token).toBeDefined();

  // Set tokens for authenticated requests
  window.localStorage.setItem("access_token", guestLogin.access_token);
  window.localStorage.setItem("refresh_token", guestLogin.refresh_token);

  // Verify guest user data
  const userResponse = await fetchUser();
  expect(userResponse.user.id).toBe(guestSignup.id);
  expect(userResponse.user.email).toBeNull();
});

test("Guest refresh token works", async () => {
  // Sign up as guest
  const guestSignup = await fetchGuestSignUp(TEST_PASSWORD!, "", TEST_CLIENT_ID!);
  const guestLogin = await fetchGuestLogin(guestSignup.id, TEST_PASSWORD!, TEST_CLIENT_ID!);

  // Set tokens for authenticated requests
  window.localStorage.setItem("access_token", guestLogin.access_token);
  window.localStorage.setItem("refresh_token", guestLogin.refresh_token);

  const refreshResponse = await refreshToken();
  expect(refreshResponse.access_token).toBeDefined();
  expect(refreshResponse.refresh_token).toBeDefined();

  // Verify the new access token works
  const userResponse = await fetchUser();
  expect(userResponse.user.id).toBe(guestSignup.id);
  expect(userResponse.user.email).toBeNull();
});

test("Guest change password keeps authenticated token state", async () => {
  const guestSignup = await fetchGuestSignUp(TEST_PASSWORD!, "", TEST_CLIENT_ID!);

  window.localStorage.setItem("access_token", guestSignup.access_token);
  window.localStorage.setItem("refresh_token", guestSignup.refresh_token);

  const newPassword = `newpass${Date.now()}`;
  await changePassword(TEST_PASSWORD!, newPassword);
  const updatedCredentials = readTransportV2Credentials(API_URL!, "user");
  expect(updatedCredentials?.accessToken).toBeTruthy();
  expect(updatedCredentials?.refreshToken).toBeTruthy();
  expect(window.localStorage.getItem("access_token")).toBeNull();
  expect(window.localStorage.getItem("refresh_token")).toBeNull();

  const userResponse = await fetchUser();
  expect(userResponse.user.id).toBe(guestSignup.id);
  expect(userResponse.user.email).toBeNull();

  const reloginResponse = await fetchGuestLogin(guestSignup.id, newPassword, TEST_CLIENT_ID!);
  expect(reloginResponse.id).toBe(guestSignup.id);
});

test("Guest logout doesn't error", async () => {
  // Sign up as guest
  const guestSignup = await fetchGuestSignUp(TEST_PASSWORD!, "", TEST_CLIENT_ID!);
  const { refresh_token } = await fetchGuestLogin(guestSignup.id, TEST_PASSWORD!, TEST_CLIENT_ID!);
  await fetchLogout(refresh_token);
});

test("Third party token generation", async () => {
  // Login first to get authenticated
  const { access_token, refresh_token } = await tryEmailLogin();
  window.localStorage.setItem("access_token", access_token);
  window.localStorage.setItem("refresh_token", refresh_token);

  // Test successful token generation with valid audience
  const validOpenSecretAudience = "https://billing-dev.opensecret.cloud";
  const opensSecretResponse = await generateThirdPartyToken(validOpenSecretAudience);
  expect(opensSecretResponse.token).toBeDefined();
  expect(typeof opensSecretResponse.token).toBe("string");
  expect(opensSecretResponse.token.length).toBeGreaterThan(0);

  // Test with any valid URL
  const anyValidUrl = "https://google.com";
  const googleResponse = await generateThirdPartyToken(anyValidUrl);
  expect(googleResponse.token).toBeDefined();
  expect(typeof googleResponse.token).toBe("string");
  expect(googleResponse.token.length).toBeGreaterThan(0);

  // Test with a non-URL audience identifier
  const stringAudience = "authenticated";
  const stringAudienceResponse = await generateThirdPartyToken(stringAudience);
  expect(stringAudienceResponse.token).toBeDefined();
  expect(typeof stringAudienceResponse.token).toBe("string");
  expect(stringAudienceResponse.token.length).toBeGreaterThan(0);

  // Test with no audience (should work)
  const noAudienceResponse = await generateThirdPartyToken();
  expect(noAudienceResponse.token).toBeDefined();
  expect(typeof noAudienceResponse.token).toBe("string");
  expect(noAudienceResponse.token.length).toBeGreaterThan(0);

  // Test reserved internal audience (this should still fail)
  try {
    await generateThirdPartyToken("access");
    throw new Error("Should not accept reserved internal audience");
  } catch (error: any) {
    expect(error.message).toBe("Bad Request");
  }
});

test("Encrypt and decrypt data", async () => {
  // Login first to get authenticated
  const { access_token, refresh_token } = await tryEmailLogin();
  window.localStorage.setItem("access_token", access_token);
  window.localStorage.setItem("refresh_token", refresh_token);

  // Test data to encrypt
  const testData = "Hello, World!";

  // Encrypt the data
  const encryptResponse = await encryptData(testData);
  expect(encryptResponse.encrypted_data).toBeDefined();
  expect(typeof encryptResponse.encrypted_data).toBe("string");
  expect(encryptResponse.encrypted_data.length).toBeGreaterThan(0);

  // Decrypt the data
  const decryptedData = await decryptData(encryptResponse.encrypted_data);
  expect(decryptedData).toBe(testData);

  // Try with a derivation path
  const derivationPath = "m/44'/0'/0'/0/0";
  const encryptWithPathResponse = await encryptData(testData, {
    private_key_derivation_path: derivationPath
  });
  expect(encryptWithPathResponse.encrypted_data).toBeDefined();

  // Decrypt with the same derivation path
  const decryptedWithPathData = await decryptData(encryptWithPathResponse.encrypted_data, {
    private_key_derivation_path: derivationPath
  });
  expect(decryptedWithPathData).toBe(testData);

  // Try decrypting with a different derivation path (should fail)
  try {
    await decryptData(encryptWithPathResponse.encrypted_data, {
      private_key_derivation_path: "m/44'/0'/0'/0/1"
    });
    throw new Error("Should not decrypt with wrong derivation path");
  } catch (error: any) {
    expect(error.message).toBe("Bad Request");
  }

  // Try decrypting with no derivation path when it was encrypted with one (should fail)
  try {
    await decryptData(encryptWithPathResponse.encrypted_data);
    throw new Error("Should not decrypt with missing derivation path");
  } catch (error: any) {
    expect(error.message).toBe("Bad Request");
  }

  // Try with invalid encrypted data
  try {
    await decryptData("invalid-data");
    throw new Error("Should not decrypt invalid data");
  } catch (error: any) {
    expect(error.message).toBe("Bad Request");
  }
});

test("Account deletion request endpoint", async () => {
  // Login first to get authenticated
  const { access_token, refresh_token } = await tryEmailLogin();
  window.localStorage.setItem("access_token", access_token);
  window.localStorage.setItem("refresh_token", refresh_token);

  // In a real app, we would generate a secure random string and hash it
  // For testing, we can use a mock hashed secret
  const mockHashedSecret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

  // This should succeed as it just sends an email and doesn't actually delete the account
  // We can't test the confirmation because we need the UUID from the email
  await requestAccountDeletion(mockHashedSecret);

  // Make sure the user is still authenticated after requesting deletion
  // If we can still fetch user data, it means the account wasn't deleted yet
  const userResponse = await fetchUser();
  expect(userResponse.user.email).toBe(TEST_EMAIL);
});

test("Instructions API - Create, List, Get, Update, Delete", async () => {
  // Login first to get authenticated
  const { access_token, refresh_token } = await tryEmailLogin();
  window.localStorage.setItem("access_token", access_token);
  window.localStorage.setItem("refresh_token", refresh_token);

  // Create a new instruction
  const instruction1 = await createInstruction({
    name: "Test Instruction 1",
    prompt: "You are a helpful assistant.",
    is_default: false
  });
  expect(instruction1.id).toBeDefined();
  expect(instruction1.object).toBe("instruction");
  expect(instruction1.name).toBe("Test Instruction 1");
  expect(instruction1.prompt).toBe("You are a helpful assistant.");
  expect(instruction1.prompt_tokens).toBeGreaterThan(0);
  expect(instruction1.is_default).toBe(false);
  expect(instruction1.created_at).toBeGreaterThan(0);
  expect(instruction1.updated_at).toBeGreaterThan(0);

  // Create a second instruction as default
  const instruction2 = await createInstruction({
    name: "Test Instruction 2",
    prompt: "You are a coding assistant that helps with TypeScript.",
    is_default: true
  });
  expect(instruction2.id).toBeDefined();
  expect(instruction2.is_default).toBe(true);

  // List instructions
  const listResponse = await listInstructions({ limit: 10 });
  expect(listResponse.object).toBe("list");
  expect(Array.isArray(listResponse.data)).toBe(true);
  expect(listResponse.data.length).toBeGreaterThanOrEqual(2);
  expect(typeof listResponse.has_more).toBe("boolean");

  // Find our created instructions in the list
  const foundInstruction1 = listResponse.data.find((i) => i.id === instruction1.id);
  const foundInstruction2 = listResponse.data.find((i) => i.id === instruction2.id);
  expect(foundInstruction1).toBeDefined();
  expect(foundInstruction2).toBeDefined();

  // Get single instruction
  const retrievedInstruction = await getInstruction(instruction1.id);
  expect(retrievedInstruction.id).toBe(instruction1.id);
  expect(retrievedInstruction.name).toBe(instruction1.name);
  expect(retrievedInstruction.prompt).toBe(instruction1.prompt);

  // Update instruction
  const updatedInstruction = await updateInstruction(instruction1.id, {
    name: "Updated Test Instruction 1",
    prompt: "You are a very helpful assistant that provides detailed explanations."
  });
  expect(updatedInstruction.id).toBe(instruction1.id);
  expect(updatedInstruction.name).toBe("Updated Test Instruction 1");
  expect(updatedInstruction.prompt).toBe(
    "You are a very helpful assistant that provides detailed explanations."
  );
  expect(updatedInstruction.prompt_tokens).toBeGreaterThan(0);

  // Set as default
  const defaultInstruction = await setDefaultInstruction(instruction1.id);
  expect(defaultInstruction.id).toBe(instruction1.id);
  expect(defaultInstruction.is_default).toBe(true);

  // Verify the other instruction is no longer default
  const instruction2Updated = await getInstruction(instruction2.id);
  expect(instruction2Updated.is_default).toBe(false);

  // Delete instructions
  const deleteResult1 = await deleteInstruction(instruction1.id);
  expect(deleteResult1.id).toBe(instruction1.id);
  expect(deleteResult1.object).toBe("instruction.deleted");
  expect(deleteResult1.deleted).toBe(true);

  const deleteResult2 = await deleteInstruction(instruction2.id);
  expect(deleteResult2.deleted).toBe(true);

  // Verify deletion - trying to get should fail
  try {
    await getInstruction(instruction1.id);
    throw new Error("Should not be able to retrieve deleted instruction");
  } catch (error: any) {
    // Expected to fail with 404 or Bad Request
    expect(error.message).toBeDefined();
  }
});

test("Instructions API - Pagination", async () => {
  // Login first to get authenticated
  const { access_token, refresh_token } = await tryEmailLogin();
  window.localStorage.setItem("access_token", access_token);
  window.localStorage.setItem("refresh_token", refresh_token);

  // Create multiple instructions for pagination testing
  const instructionIds = [];
  for (let i = 0; i < 5; i++) {
    const instruction = await createInstruction({
      name: `Pagination Test Instruction ${i}`,
      prompt: `This is test instruction number ${i}.`,
      is_default: false
    });
    instructionIds.push(instruction.id);
  }

  // Test pagination with limit
  const page1 = await listInstructions({ limit: 2 });
  expect(page1.data.length).toBeLessThanOrEqual(2);

  // Test forward pagination with after cursor
  if (page1.last_id) {
    const page2 = await listInstructions({
      limit: 2,
      after: page1.last_id
    });
    expect(Array.isArray(page2.data)).toBe(true);
  }

  // Test ordering
  const descOrder = await listInstructions({ order: "desc", limit: 5 });
  expect(descOrder.data.length).toBeGreaterThan(0);

  // Clean up
  for (const id of instructionIds) {
    await deleteInstruction(id);
  }
});

test("Instructions API - Error cases", async () => {
  // Login first to get authenticated
  const { access_token, refresh_token } = await tryEmailLogin();
  window.localStorage.setItem("access_token", access_token);
  window.localStorage.setItem("refresh_token", refresh_token);

  // Test getting non-existent instruction
  try {
    await getInstruction("00000000-0000-0000-0000-000000000000");
    throw new Error("Should not be able to get non-existent instruction");
  } catch (error: any) {
    expect(error.message).toBeDefined();
  }

  // Test updating non-existent instruction
  try {
    await updateInstruction("00000000-0000-0000-0000-000000000000", {
      name: "Should fail"
    });
    throw new Error("Should not be able to update non-existent instruction");
  } catch (error: any) {
    expect(error.message).toBeDefined();
  }

  // Test deleting non-existent instruction
  try {
    await deleteInstruction("00000000-0000-0000-0000-000000000000");
    throw new Error("Should not be able to delete non-existent instruction");
  } catch (error: any) {
    expect(error.message).toBeDefined();
  }

  // Test setting non-existent instruction as default
  try {
    await setDefaultInstruction("00000000-0000-0000-0000-000000000000");
    throw new Error("Should not be able to set non-existent instruction as default");
  } catch (error: any) {
    expect(error.message).toBeDefined();
  }
});

test("KV Store - Delete All", async () => {
  // Login first to get authenticated
  const { access_token, refresh_token } = await tryEmailLogin();
  window.localStorage.setItem("access_token", access_token);
  window.localStorage.setItem("refresh_token", refresh_token);

  // Add multiple keys
  await fetchPut("key1", "value1");
  await fetchPut("key2", "value2");
  await fetchPut("key3", "value3");

  // Verify they exist
  const listBefore = await fetchList();
  expect(listBefore.length).toBeGreaterThanOrEqual(3);

  // Delete all keys
  await fetchDeleteAllKV();

  // Verify they are gone
  const listAfter = await fetchList();
  expect(listAfter.length).toBe(0);
});
