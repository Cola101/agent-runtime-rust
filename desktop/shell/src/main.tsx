import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { all } from "./surfaces/registry";
import "./app.css";
import "./runtime";
import "./surfaces/Chat";
import "./surfaces/Conversations";
import "./surfaces/Workspace";

// A dev-server-only bridge, so every surface can be looked at in a browser with
// content in it. Never reached by a packaged build: `import.meta.env.DEV` is
// false there and vite drops the branch, so the module is not in the bundle.
if (import.meta.env.DEV && new URLSearchParams(location.search).has("fake")) {
  const { installDevBridge } = await import("./dev/bridge");
  installDevBridge();
}

const root = document.getElementById("root");
if (!root) throw new Error("index.html is missing #root");

createRoot(root).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);

// Tell the host the shell actually rendered. In a plain browser there is no
// host to tell, and that is not an error.
window.desk?.mounted(all().length);
