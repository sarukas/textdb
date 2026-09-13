import { createRoot } from "react-dom/client";
import { App } from "./App";
import { ToastProvider } from "./components/Toasts";
import "./styles.css";

createRoot(document.getElementById("root")!).render(
  <ToastProvider>
    <App />
  </ToastProvider>,
);
