import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { all } from "./surfaces/registry";
import "./app.css";
import "./surfaces/Chat";

const root = document.getElementById("root");
if (!root) throw new Error("index.html is missing #root");

createRoot(root).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);

// Tell the host the shell actually rendered. In a plain browser there is no
// host to tell, and that is not an error.
declare global {
  interface Window {
    desk?: { mounted(surfaces: number): void; endpoint(): Promise<string | null> };
  }
}
window.desk?.mounted(all().length);
