"use strict";

// The page's window.ipc.postMessage, as wry gives it in the JTokens window.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("ipc", { postMessage: (body) => ipcRenderer.send("tk", String(body)) });
