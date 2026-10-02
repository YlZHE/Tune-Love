import React from "react";
import ReactDOM from "react-dom/client";
import "@radix-ui/themes/styles.css";
import "./styles.css";
import { App } from "./App";
import { SettingsPage } from "./SettingsPage";

const settingsView = new URLSearchParams(window.location.search).get("view") === "settings";
document.title = settingsView ? "Tune Love · 设置" : "Tune Love";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>{settingsView ? <SettingsPage /> : <App />}</React.StrictMode>,
);
