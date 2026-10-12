import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { initFont } from "./font";
import "./styles.css";

// 渲染前套用已保存的界面字体，避免首帧用默认字体再跳变
initFont();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);
