import React from "react";
import ReactDOM from "react-dom/client";
import { OpenSecretProvider } from "@mapleai/sdk";
import { openSecretClientConfig } from "@/config/openSecretClientConfig";
import { AuthSite } from "./AuthSite";
import "./style.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <OpenSecretProvider {...openSecretClientConfig()}>
      <AuthSite />
    </OpenSecretProvider>
  </React.StrictMode>
);
