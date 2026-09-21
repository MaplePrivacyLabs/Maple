const shared = require("./tailwind.config.cjs");

/** @type {import('tailwindcss').Config} */
module.exports = {
  ...shared,
  content: [
    "./auth.html",
    "./src/auth-site/**/*.{ts,tsx}",
    "!./src/auth-site/**/*.test.{ts,tsx}",
    "!./src/auth-site/fixtures/**",
    "./src/components/ui/button.tsx",
    "./src/components/HostedNativeSignInConfirmation.tsx"
  ]
};
