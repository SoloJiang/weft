import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

let cached: Update | null = null;

export async function checkUpdate() {
  try {
    const update = await check();
    if (update) {
      cached = update;
      return {
        version: update.version,
        body: update.body ?? "",
      };
    }
  } catch {
    // Silently fail in dev or when offline.
  }
  return null;
}

export async function installUpdate() {
  if (!cached) return;
  await cached.downloadAndInstall();
  await relaunch();
}

export function dismissUpdate() {
  cached = null;
}
